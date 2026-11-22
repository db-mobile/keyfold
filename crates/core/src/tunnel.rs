//! SSH tunnels (CONCEPT 8 and 11, "SSH tunnels"): a profile can name a
//! bastion, and every connection the connection manager opens for it then
//! runs through one SSH session to that host.
//!
//! Cluster nodes, redirect targets and replicas are addresses the server
//! announces, so the tunnel does not forward one fixed port: it forwards
//! any `host:port` on demand, and the name is resolved on the bastion. The
//! mechanism is the client crate's resolver hook. For each connect the
//! tunnel opens a `direct-tcpip` channel to the real address, binds a port
//! on 127.0.0.1 that accepts exactly one connection, and hands that port to
//! the client crate, which connects to it and keeps doing TLS against the
//! real host name. The listener is gone after that one connection, so the
//! forward is not left open to other local processes.
//!
//! The host key is checked against the user's `~/.ssh/known_hosts` (read,
//! never written) and a file of the app's own. An unknown key fails the
//! connect with [`Error::HostKey`], which carries the fingerprint for the
//! user to accept; [`trust_host_key`] then records it in the app's file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;

use crate::error::{Error, Result};

/// How the tunnel proves who the user is.
#[derive(Clone, PartialEq, Eq)]
pub enum SshAuth {
    /// The keys of the running SSH agent (`SSH_AUTH_SOCK`), one after another.
    Agent,
    /// A private key file, with its passphrase when it has one.
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    Password(String),
}

/// The bastion of a profile.
#[derive(Clone, PartialEq, Eq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// The app's own known-hosts file; keys the user accepted are in it.
    pub known_hosts: Option<PathBuf>,
}

impl std::fmt::Debug for SshAuth {
    /// Never prints a password or a passphrase.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshAuth::Agent => write!(f, "agent"),
            SshAuth::KeyFile { path, passphrase } => {
                write!(
                    f,
                    "key {}{}",
                    path.display(),
                    if passphrase.is_some() { " (passphrase)" } else { "" }
                )
            }
            SshAuth::Password(_) => write!(f, "password"),
        }
    }
}

impl std::fmt::Debug for SshConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}:{} ({:?})", self.user, self.host, self.port, self.auth)
    }
}

impl SshConfig {
    /// `user@host[:port]`, as on a command line.
    pub fn parse(text: &str, auth: SshAuth) -> Result<SshConfig> {
        let bad = || Error::Config(format!("not user@host[:port]: {text:?}"));
        let (user, rest) = text.split_once('@').ok_or_else(bad)?;
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| bad())?),
            None => (rest, 22),
        };
        if user.is_empty() || host.is_empty() {
            return Err(bad());
        }
        Ok(SshConfig {
            host: host.to_string(),
            port,
            user: user.to_string(),
            auth,
            known_hosts: None,
        })
    }
}

/// A host key the tunnel did not accept, for the user to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// `SHA256:...`
    pub fingerprint: String,
    /// The key in OpenSSH form, what [`trust_host_key`] takes.
    pub openssh: String,
    /// A different key is on record for this host: refused, not offered.
    pub changed: bool,
}

/// Record a host key the user accepted in the app's known-hosts file.
pub fn trust_host_key(file: &Path, key: &HostKey) -> Result<()> {
    if key.changed {
        return Err(Error::Tunnel(format!(
            "the key of {}:{} differs from the one on record; remove the old entry by hand if the change is expected",
            key.host, key.port
        )));
    }
    let public = keys::PublicKey::from_openssh(&key.openssh).map_err(|e| Error::Tunnel(format!("not a public key: {e}")))?;
    // host keys another local user could add to would defeat the check
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        crate::private::create_dir(dir)?;
    }
    crate::private::create_file(file)?;
    keys::known_hosts::learn_known_hosts_path(&key.host, key.port, &public, file)
        .map_err(|e| Error::Tunnel(format!("could not write {}: {e}", file.display())))
}

/// What the SSH client is told about the server it reached.
struct Verifier {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    /// The key that was not accepted, for the error.
    refused: Arc<Mutex<Option<HostKey>>>,
}

impl client::Handler for Verifier {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        let mut changed = false;
        // the user's own file first, then the app's; a file that is missing is no match
        let user_file = keys::check_known_hosts(&self.host, self.port, &key);
        let app_file = match &self.known_hosts {
            Some(path) if path.exists() => keys::check_known_hosts_path(&self.host, self.port, &key, path),
            _ => Ok(false),
        };
        for outcome in [user_file, app_file] {
            match outcome {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(keys::Error::KeyChanged { .. }) => changed = true,
                Err(_) => {}
            }
        }
        if let Ok(mut slot) = self.refused.lock() {
            *slot = Some(HostKey {
                host: self.host.clone(),
                port: self.port,
                fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                openssh: key.to_openssh().unwrap_or_default(),
                changed,
            });
        }
        Ok(false)
    }
}

type Session = Arc<Handle<Verifier>>;

/// One SSH session to a bastion, opened on first use and again after it
/// was lost, shared by every connection of a profile.
pub struct Tunnel {
    config: SshConfig,
    session: tokio::sync::Mutex<Option<Session>>,
    forwards: AtomicU64,
    logins: AtomicU64,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tunnel({:?}, {} forwards)", self.config, self.forwards())
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the one-shot listener waits for the client crate to connect.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

fn tunnel_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Tunnel(format!("{what}: {e}"))
}

impl Tunnel {
    /// A tunnel that logs in when the first connection asks for it.
    pub fn new(config: SshConfig) -> Arc<Tunnel> {
        Arc::new(Tunnel {
            config,
            session: tokio::sync::Mutex::new(None),
            forwards: AtomicU64::new(0),
            logins: AtomicU64::new(0),
        })
    }

    /// A tunnel that is logged in, so a wrong password or an unknown host
    /// key is reported before anything else happens.
    pub async fn open(config: SshConfig) -> Result<Arc<Tunnel>> {
        let tunnel = Tunnel::new(config);
        tunnel.session().await?;
        Ok(tunnel)
    }

    /// Log in if the tunnel is not open (or no longer is).
    pub async fn ensure(&self) -> Result<()> {
        self.session().await.map(|_| ())
    }

    pub fn config(&self) -> &SshConfig {
        &self.config
    }

    /// Connections forwarded so far.
    pub fn forwards(&self) -> u64 {
        self.forwards.load(Ordering::Relaxed)
    }

    /// SSH logins so far: one, unless the session was lost in between.
    pub fn logins(&self) -> u64 {
        self.logins.load(Ordering::Relaxed)
    }

    /// The live session, logging in (again) when there is none.
    async fn session(&self) -> Result<Session> {
        let mut slot = self.session.lock().await;
        if let Some(s) = slot.as_ref()
            && !s.is_closed()
        {
            return Ok(Arc::clone(s));
        }
        let session = Arc::new(self.login().await?);
        self.logins.fetch_add(1, Ordering::Relaxed);
        *slot = Some(Arc::clone(&session));
        Ok(session)
    }

    async fn login(&self) -> Result<Handle<Verifier>> {
        let c = &self.config;
        let at = format!("{}@{}:{}", c.user, c.host, c.port);
        let refused = Arc::new(Mutex::new(None));
        let verifier = Verifier {
            host: c.host.clone(),
            port: c.port,
            known_hosts: c.known_hosts.clone(),
            refused: Arc::clone(&refused),
        };
        let config = Arc::new(client::Config {
            nodelay: true,
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });
        let connect = client::connect(config, (c.host.as_str(), c.port), verifier);
        let mut session = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Err(_) => {
                return Err(Error::Tunnel(format!(
                    "SSH to {at}: no answer within {} s",
                    CONNECT_TIMEOUT.as_secs()
                )));
            }
            Ok(Err(e)) => {
                // the handshake stopped at the host key: say which key
                if let Some(key) = refused.lock().ok().and_then(|mut k| k.take()) {
                    return Err(Error::HostKey(Box::new(key)));
                }
                return Err(tunnel_err(&format!("SSH to {at}"), e));
            }
            Ok(Ok(s)) => s,
        };
        let accepted = match &c.auth {
            SshAuth::Password(password) => session
                .authenticate_password(&c.user, password)
                .await
                .map_err(|e| tunnel_err(&format!("SSH login as {at}"), e))?
                .success(),
            SshAuth::KeyFile { path, passphrase } => {
                let key = keys::load_secret_key(path, passphrase.as_deref())
                    .map_err(|e| tunnel_err(&format!("the key file {}", path.display()), e))?;
                let hash = session
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| tunnel_err(&format!("SSH login as {at}"), e))?
                    .flatten();
                session
                    .authenticate_publickey(&c.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                    .map_err(|e| tunnel_err(&format!("SSH login as {at}"), e))?
                    .success()
            }
            SshAuth::Agent => agent_login(&mut session, &c.user, &at).await?,
        };
        if !accepted {
            return Err(Error::Tunnel(format!("SSH login as {at} was not accepted")));
        }
        tracing::info!(bastion = %at, "ssh tunnel open");
        Ok(session)
    }

    /// A port on 127.0.0.1 that carries exactly one connection to
    /// `host:port` as the bastion sees it. The channel is opened before
    /// this returns, so an address the bastion cannot reach is an error
    /// here and not a connection that hangs.
    pub async fn forward(&self, host: &str, port: u16) -> Result<SocketAddr> {
        // a redirect names an IPv6 host in brackets
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let via = format!("{}:{}", self.config.host, self.config.port);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(Error::Io)?;
        let local = listener.local_addr().map_err(Error::Io)?;
        let mut session = self.session().await?;
        let mut channel = session
            .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", u32::from(local.port()))
            .await;
        if channel.is_err() && session.is_closed() {
            // the session died since it was last used: one new login
            session = self.session().await?;
            channel = session
                .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", u32::from(local.port()))
                .await;
        }
        let channel = channel.map_err(|e| tunnel_err(&format!("{host}:{port} could not be reached through {via}"), e))?;
        self.forwards.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            // one connection, from this process, then the port is closed
            let accepted = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()).await;
            drop(listener);
            let Ok(Ok((mut socket, _))) = accepted else { return };
            let _ = socket.set_nodelay(true);
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
        Ok(local)
    }
}

#[cfg(unix)]
async fn agent_login(session: &mut Handle<Verifier>, user: &str, at: &str) -> Result<bool> {
    let mut agent = keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| tunnel_err("the SSH agent (SSH_AUTH_SOCK)", e))?;
    let identities = agent.request_identities().await.map_err(|e| tunnel_err("the SSH agent", e))?;
    if identities.is_empty() {
        return Err(Error::Tunnel("the SSH agent holds no keys".into()));
    }
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| tunnel_err(&format!("SSH login as {at}"), e))?
        .flatten();
    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = if key.algorithm().is_rsa() { hash } else { None };
        match session.authenticate_publickey_with(user, key, hash, &mut agent).await {
            Ok(r) if r.success() => return Ok(true),
            Ok(_) => {}
            Err(e) => return Err(tunnel_err(&format!("SSH login as {at} with a key of the agent"), e)),
        }
    }
    Ok(false)
}

#[cfg(not(unix))]
async fn agent_login(_session: &mut Handle<Verifier>, _user: &str, _at: &str) -> Result<bool> {
    Err(Error::Tunnel(
        "the SSH agent is not supported on this platform yet; use a key file or a password".into(),
    ))
}

/// The tunnel as the client crate's resolver: every address it asks for
/// becomes a one-shot local port (see the module documentation).
pub(crate) struct TunnelResolver(pub Arc<Tunnel>);

impl redis::io::AsyncDNSResolver for TunnelResolver {
    fn resolve<'a, 'b: 'a>(&'a self, host: &'b str, port: u16) -> redis::RedisFuture<'a, Box<dyn Iterator<Item = SocketAddr> + Send + 'a>> {
        Box::pin(async move {
            match self.0.forward(host, port).await {
                Ok(addr) => Ok(Box::new(std::iter::once(addr)) as Box<dyn Iterator<Item = SocketAddr> + Send>),
                // a transport error to the client crate: no RESP2 retry, and the router reconnects once
                Err(e) => Err(redis::RedisError::from(std::io::Error::other(e.to_string()))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bastion_is_written_as_user_at_host() {
        let c = SshConfig::parse("ops@bastion.example:2222", SshAuth::Agent).unwrap();
        assert_eq!((c.user.as_str(), c.host.as_str(), c.port), ("ops", "bastion.example", 2222));
        assert_eq!(SshConfig::parse("ops@10.0.0.1", SshAuth::Agent).unwrap().port, 22);
        for bad in ["bastion", "@host", "user@", "user@host:x"] {
            assert!(SshConfig::parse(bad, SshAuth::Agent).is_err(), "{bad}");
        }
    }

    #[test]
    fn secrets_never_reach_a_debug_line() {
        let mut c = SshConfig::parse("ops@bastion:22", SshAuth::Password("hunter2".into())).unwrap();
        assert_eq!(format!("{c:?}"), "ops@bastion:22 (password)");
        c.auth = SshAuth::KeyFile {
            path: "/home/ops/.ssh/id_ed25519".into(),
            passphrase: Some("correct horse".into()),
        };
        let shown = format!("{c:?} {:?}", Tunnel::new(c.clone()));
        assert!(
            !shown.contains("correct horse") && shown.contains("id_ed25519 (passphrase)"),
            "{shown}"
        );
    }

    #[test]
    fn a_changed_host_key_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("known_hosts");
        let key = HostKey {
            host: "bastion".into(),
            port: 22,
            fingerprint: "SHA256:x".into(),
            openssh: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti".into(),
            changed: true,
        };
        assert!(trust_host_key(&file, &key).is_err());
        assert!(!file.exists());
        let key = HostKey { changed: false, ..key };
        trust_host_key(&file, &key).unwrap();
        let public = keys::PublicKey::from_openssh(&key.openssh).unwrap();
        assert!(keys::check_known_hosts_path("bastion", 22, &public, &file).unwrap());
        assert!(!keys::check_known_hosts_path("other", 22, &public, &file).unwrap());
        assert!(
            trust_host_key(
                &file,
                &HostKey {
                    openssh: "nonsense".into(),
                    ..key
                }
            )
            .is_err()
        );
    }
}
