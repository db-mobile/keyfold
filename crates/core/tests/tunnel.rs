//! Integration tests of the SSH tunnel (CONCEPT 8 and 11). They need the
//! bastion and the server behind it that `scripts/servers.sh` starts:
//! `KEYFOLD_TEST_SSH` (`user@host:port`), `KEYFOLD_TEST_SSH_KEY` (a private
//! key the bastion accepts; a second one with the passphrase
//! `keyfold-pass` sits beside it as `<key>_pass`), `KEYFOLD_TEST_SSH_PASSWORD`
//! and `KEYFOLD_TEST_SSH_URL`, a server whose name only the bastion can
//! resolve. Its database is flushed.

use std::path::PathBuf;
use std::time::Duration;

use keyfold_core::commands::{self, testing};
use keyfold_core::index::{Index, IndexOptions, Location};
use keyfold_core::route::Hop;
use keyfold_core::scan::{self, ScanConfig, ScanPhase, ScanProfile, targets};
use keyfold_core::tunnel::{HostKey, SshAuth, SshConfig, Tunnel, trust_host_key};
use keyfold_core::{ConnectionManager, Error, Profile, Router, ScanTarget, capability, routed};
use redis::Value;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Bastion {
    at: String,
    key: PathBuf,
    password: String,
    url: String,
    /// A known-hosts file of this test alone.
    dir: tempfile::TempDir,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Bastion {
    fn known_hosts(&self) -> PathBuf {
        self.dir.path().join("known_hosts")
    }

    fn config(&self, auth: SshAuth) -> SshConfig {
        let mut c = SshConfig::parse(&self.at, auth).unwrap();
        c.known_hosts = Some(self.known_hosts());
        c
    }

    fn key_auth(&self) -> SshAuth {
        SshAuth::KeyFile {
            path: self.key.clone(),
            passphrase: None,
        }
    }

    fn profile(&self, url: &str, auth: SshAuth) -> Profile {
        let mut p = Profile::from_url(url).unwrap();
        p.ssh = Some(self.config(auth));
        p
    }

    /// Accept the bastion's host key, as the user would after reading it.
    async fn trust(&self) -> HostKey {
        match Tunnel::open(self.config(self.key_auth())).await {
            Err(Error::HostKey(key)) => {
                assert!(!key.changed, "{key:?}");
                trust_host_key(&self.known_hosts(), &key).unwrap();
                *key
            }
            Ok(_) => panic!("the host key of the test bastion is already in ~/.ssh/known_hosts; the test cannot show the first contact"),
            Err(e) => panic!("{e}"),
        }
    }
}

async fn bastion() -> Option<Bastion> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(at), Some(key), Some(password), Some(url)) = (
        var("KEYFOLD_TEST_SSH"),
        var("KEYFOLD_TEST_SSH_KEY"),
        var("KEYFOLD_TEST_SSH_PASSWORD"),
        var("KEYFOLD_TEST_SSH_URL"),
    ) else {
        eprintln!("KEYFOLD_TEST_SSH and its companions are not set; skipping the tunnel test");
        return None;
    };
    Some(Bastion {
        at,
        key: key.into(),
        password,
        url,
        dir: tempfile::tempdir().unwrap(),
        _guard: LOCK.lock().await,
    })
}

async fn fill(conn: &mut keyfold_core::Conn, n: usize) {
    conn.raw(&testing::flushdb()).await.unwrap();
    let mut pipe = commands::pipeline();
    for i in 0..n {
        pipe.add_command(testing::set(format!("tun:{}:{i}", i % 7).as_bytes(), b"v"));
    }
    for r in conn.pipeline(&pipe).await.unwrap() {
        r.unwrap();
    }
}

/// A server whose name the test process cannot even resolve is browsed
/// through the bastion: host key first, then scan, read and write, all on
/// one SSH login.
#[tokio::test]
async fn a_server_behind_a_bastion_is_reached_through_the_tunnel() {
    let Some(b) = bastion().await else { return };

    // without the tunnel there is no way to it
    let direct = ConnectionManager::new(Profile::from_url(&b.url).unwrap()).with_timeouts(Duration::from_secs(3), Duration::from_secs(3));
    assert!(direct.connect().await.is_err());

    // first contact: the connect stops at the host key and shows it
    let manager = ConnectionManager::new(b.profile(&b.url, b.key_auth()));
    let key = match manager.connect().await {
        Err(Error::HostKey(key)) => key,
        other => panic!("expected the host key question, got {:?}", other.map(|c| c.label().to_string())),
    };
    assert!(key.fingerprint.starts_with("SHA256:") && !key.changed, "{key:?}");
    assert!(Error::HostKey(key.clone()).to_string().contains(&key.fingerprint));
    trust_host_key(&b.known_hosts(), &key).unwrap();

    // accepted: connect, probe, fill, scan into an index, read and write
    let mut conn = manager.connect().await.unwrap();
    assert_eq!(conn.label(), "inner:6379/0", "the address stays the server's own");
    let caps = capability::probe(&mut conn, 0).await.unwrap();
    assert!(!caps.version.is_empty());
    fill(&mut conn, 3000).await;
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let out = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    assert_eq!(out.progress.phase, ScanPhase::Complete, "{:?}", out.progress);
    assert_eq!((out.progress.scanned, out.index.key_count()), (3000, 3000));
    let mut router = Router::new(manager.clone(), manager.connect().await.unwrap(), &caps).await;
    let got = routed!(router, Hop::key(b"tun:1:1"), |c| c.raw(&commands::exists(&[b"tun:1:1"])).await).unwrap();
    assert_eq!(got, Value::Int(1));

    // every connection was a forward of its own, on one login
    let tunnel = manager.tunnel().expect("a tunnelled manager");
    assert_eq!(tunnel.logins(), 1);
    assert!(tunnel.forwards() >= 3, "{tunnel:?}");
    // a clone shares it
    let clone = manager.clone();
    clone.connect().await.unwrap();
    assert_eq!(tunnel.logins(), 1);

    // an address the bastion cannot reach either is said so, quickly
    let nowhere = ConnectionManager::new(b.profile("redis://nowhere.invalid:6379/0", b.key_auth()));
    let err = match nowhere.connect().await {
        Err(e) => e.to_string(),
        Ok(c) => panic!("connected to {}", c.label()),
    };
    assert!(
        err.contains("nowhere.invalid:6379") && err.contains("could not be reached through"),
        "{err}"
    );
    conn.raw(&testing::flushdb()).await.unwrap();
}

/// The ways to log in, and the ways it fails.
#[tokio::test]
async fn the_bastion_checks_who_logs_in_and_the_tunnel_checks_the_bastion() {
    let Some(b) = bastion().await else { return };
    let key = b.trust().await;

    // a password, a key, a key with a passphrase
    let pass_key = PathBuf::from(format!("{}_pass", b.key.display()));
    for auth in [
        SshAuth::Password(b.password.clone()),
        b.key_auth(),
        SshAuth::KeyFile {
            path: pass_key.clone(),
            passphrase: Some("keyfold-pass".into()),
        },
    ] {
        let manager = ConnectionManager::new(b.profile(&b.url, auth.clone()));
        let mut conn = manager.connect().await.unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(conn.raw(&commands::dbsize()).await.unwrap(), Value::Int(_)));
    }

    // refused logins are errors of the tunnel, with the reason
    let refused = |auth: SshAuth| {
        let config = b.config(auth);
        async move { Tunnel::open(config).await.expect_err("refused").to_string() }
    };
    let e = refused(SshAuth::Password("not the password".into())).await;
    assert!(e.contains("was not accepted"), "{e}");
    let e = refused(SshAuth::KeyFile {
        path: pass_key.clone(),
        passphrase: Some("wrong".into()),
    })
    .await;
    assert!(e.contains("key file"), "{e}");
    let e = refused(SshAuth::KeyFile {
        path: b.dir.path().join("no-such-key"),
        passphrase: None,
    })
    .await;
    assert!(e.contains("no-such-key"), "{e}");
    let mut wrong_port = b.config(b.key_auth());
    wrong_port.port = 1;
    let e = Tunnel::open(wrong_port).await.expect_err("nothing listens there").to_string();
    assert!(e.contains("SSH to"), "{e}");

    // another key on record for this host: refused, and never offered for trust
    let other = tempfile::tempdir().unwrap();
    let file = other.path().join("known_hosts");
    let impostor = HostKey {
        openssh: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti".into(),
        ..key.clone()
    };
    trust_host_key(&file, &impostor).unwrap();
    let mut config = b.config(b.key_auth());
    config.known_hosts = Some(file.clone());
    match Tunnel::open(config).await {
        Err(Error::HostKey(k)) => {
            assert!(k.changed, "{k:?}");
            assert_eq!(k.fingerprint, key.fingerprint, "the key shown is the one the server presented");
            assert!(Error::HostKey(k.clone()).to_string().contains("has changed"));
            assert!(trust_host_key(&file, &k).is_err());
        }
        other => panic!("a changed host key must stop the login: {:?}", other.map(|t| t.logins())),
    }
}

/// Live updates through the tunnel: the subscription is a RESP3
/// connection with push messages, because the dedicated pub/sub client
/// cannot be sent through the tunnel.
#[tokio::test]
async fn keyspace_notifications_arrive_through_the_tunnel() {
    use keyfold_core::notify::{self, LiveConfig, LivePhase};
    let Some(b) = bastion().await else { return };
    b.trust().await;
    let manager = ConnectionManager::new(b.profile(&b.url, b.key_auth()));
    let mut conn = manager.connect().await.unwrap();
    conn.raw(&testing::flushdb()).await.unwrap();
    let before = notify::setting(&mut conn).await.unwrap();
    if before.keyevents_enabled() != Some(true) {
        notify::enable(&mut conn).await.unwrap();
    }

    let (handle, mut events) = notify::start(
        manager.clone(),
        vec![manager.profile().addr()],
        0,
        LiveConfig {
            flush_every: Duration::from_millis(50),
            ..LiveConfig::default()
        },
    );
    let mut status = handle.status();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            status.changed().await.unwrap();
            if status.borrow().phase == LivePhase::Live {
                break;
            }
        }
    })
    .await
    .expect("the subscription went live through the tunnel");

    conn.raw(&testing::set(b"live:\xff:new", b"1")).await.unwrap();
    conn.raw(&testing::set(b"live:gone", b"1")).await.unwrap();
    conn.raw(&testing::del(&[b"live:gone"])).await.unwrap();
    let mut seen: Vec<notify::Event> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && seen.len() < 3 {
        if let Ok(Some(batch)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            seen.extend(batch);
        }
    }
    let of = |key: &[u8]| {
        seen.iter()
            .filter(|e| e.key == key)
            .map(|e| (e.event.as_str(), e.gone))
            .collect::<Vec<_>>()
    };
    assert_eq!(of(b"live:\xff:new"), [("set", false)], "{seen:?}");
    assert_eq!(of(b"live:gone"), [("set", false), ("del", true)], "{seen:?}");
    handle.stop();
    assert_eq!(manager.tunnel().unwrap().logins(), 1);
    conn.raw(&testing::flushdb()).await.unwrap();
}

/// A cluster through the tunnel: every node the server announces, the
/// replicas a scan prefers included, is reached by a forward of its own.
#[tokio::test]
async fn a_cluster_is_reached_node_by_node_through_the_tunnel() {
    let Some(b) = bastion().await else { return };
    let Ok(url) = std::env::var("KEYFOLD_TEST_CLUSTER_URL") else {
        eprintln!("KEYFOLD_TEST_CLUSTER_URL not set; skipping");
        return;
    };
    b.trust().await;
    let manager = ConnectionManager::new(b.profile(&url, b.key_auth()));
    let tunnel = std::sync::Arc::clone(manager.tunnel().unwrap());
    let mut conn = manager.connect().await.unwrap();
    let caps = capability::probe(&mut conn, 0).await.unwrap();
    assert!(caps.is_cluster());
    let mut router = Router::new(manager.clone(), conn, &caps).await;
    assert!(router.is_cluster(), "{:?}", router.note());
    let before = tunnel.forwards();

    // one key per shard at least: each owner is an address the server announced
    let mut owners = std::collections::BTreeSet::new();
    for i in 0..40 {
        let key = format!("tunnel:{i}");
        let cmd = testing::set(key.as_bytes(), b"v");
        routed!(router, Hop::key(key.as_bytes()), |c| c.raw(&cmd).await).unwrap();
        owners.insert(router.addr_for_key(key.as_bytes()).unwrap());
    }
    assert_eq!(owners.len(), 3, "{owners:?}");
    assert!(
        tunnel.forwards() >= before + 2,
        "the other primaries were reached through the tunnel: {tunnel:?}"
    );

    // a scan reads one node per shard, replicas when they are there
    let t = router.topology().unwrap().clone();
    let plan = targets(&manager, &caps, Some(&t), ScanTarget::Auto, ScanProfile::Fast)
        .await
        .unwrap();
    assert_eq!(plan.nodes.len(), 3, "{:?}", plan.notes);
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let out = scan::start_nodes(plan.nodes, index, ScanConfig::default()).wait().await.unwrap();
    assert_eq!(out.progress.phase, ScanPhase::Complete, "{:?}", out.progress);
    assert!(out.index.key_count() >= 1, "replicas may lag behind the writes of a moment ago");
    assert!(tunnel.forwards() >= before + 5, "{tunnel:?}");
    assert_eq!(tunnel.logins(), 1);
    router.flush(false).await.unwrap();
}
