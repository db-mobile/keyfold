//! Integration tests against a live server. Set `KEYFOLD_TEST_URL`
//! (for example `redis://127.0.0.1:6379/15`) to run them; CI runs this
//! file once per server in the matrix. The database is flushed.

use std::time::Duration;

use keyfold_core::commands::{self, testing};
use keyfold_core::index::{Index, IndexOptions, Location, Sort};
use keyfold_core::scan::{self, ScanConfig, ScanPhase};
use keyfold_core::{Conn, ConnectionManager, Profile, Router, ScanStatus, capability};
use redis::Value;

/// All tests share one database, so they run one at a time.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn test_url() -> Option<(String, tokio::sync::MutexGuard<'static, ()>)> {
    match std::env::var("KEYFOLD_TEST_URL") {
        Ok(u) if !u.is_empty() => Some((u, DB_LOCK.lock().await)),
        _ => {
            eprintln!("KEYFOLD_TEST_URL not set; skipping live-server test");
            None
        }
    }
}

async fn connect(url: &str) -> (ConnectionManager, Conn) {
    let profile = Profile::from_url(url).unwrap();
    let manager = ConnectionManager::new(profile);
    let conn = manager.connect().await.expect("connect");
    (manager, conn)
}

async fn fill(conn: &mut Conn, keys: &[String]) {
    conn.raw(&testing::flushdb()).await.unwrap();
    for chunk in keys.chunks(5000) {
        let mut pipe = commands::pipeline();
        for k in chunk {
            pipe.add_command(testing::set(k.as_bytes(), b"v"));
        }
        for r in conn.pipeline(&pipe).await.unwrap() {
            r.unwrap();
        }
    }
}

fn sample_keys(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 4 {
            0 => format!("user:{}:profile", i / 4),
            1 => format!("user:{}:cart", i / 4),
            2 => format!("order:{:032x}", i),
            _ => format!("event:{i}"),
        })
        .collect()
}

#[tokio::test]
async fn capability_probe_identifies_server() {
    let Some((url, _guard)) = test_url().await else { return };
    let (_m, mut conn) = connect(&url).await;
    let caps = capability::probe(&mut conn, 0).await.unwrap();
    assert!(
        matches!(caps.server, keyfold_core::ServerKind::Redis | keyfold_core::ServerKind::Valkey),
        "{:?}",
        caps.server
    );
    assert!(!caps.version.is_empty());
    assert!(caps.dbsize.is_some(), "DBSIZE should be allowed in tests");
    assert!(caps.has("scan"));
    assert!(caps.has("memory|usage"));
    assert!(caps.has("object|encoding"));
    assert!(caps.has("unlink"));
    assert_eq!(caps.protocol, 3);
    if let Ok(expect) = std::env::var("KEYFOLD_EXPECT_SERVER") {
        assert_eq!(caps.server.to_string(), expect);
    }
}

#[tokio::test]
async fn raw_errors_are_classified() {
    let Some((url, _guard)) = test_url().await else { return };
    let (_m, mut conn) = connect(&url).await;
    let mut bad = redis::cmd("SCAN");
    bad.arg("notacursor");
    let err = conn.raw(&bad).await.unwrap_err();
    assert!(matches!(err, keyfold_core::Error::Server { .. }), "{err:?}");
    // pipelines surface per-command errors in place
    let mut pipe = commands::pipeline();
    pipe.add_command(commands::dbsize());
    pipe.add_command(bad);
    pipe.add_command(commands::dbsize());
    pipe.ignore_errors();
    let replies = conn.pipeline(&pipe).await.unwrap();
    assert_eq!(replies.len(), 3);
    assert!(replies[0].is_ok());
    assert!(replies[1].is_err());
    assert!(replies[2].is_ok());
}

#[tokio::test]
async fn full_scan_builds_an_exact_index() {
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(20_000);
    fill(&mut conn, &keys).await;

    let index = Index::open(Location::Memory, &IndexOptions::default()).unwrap();
    let scan_conn = manager.connect().await.unwrap();
    let handle = scan::start(scan_conn, index, ScanConfig::default());
    let outcome = handle.wait().await.unwrap();
    let p = &outcome.progress;
    assert_eq!(p.phase, ScanPhase::Complete);
    assert_eq!(p.dbsize, Some(keys.len() as u64));
    assert!(p.scanned >= keys.len() as u64);
    assert_eq!(p.indexed_new, keys.len() as u64);
    assert!(p.calls >= 1);

    let index = outcome.index;
    assert_eq!(index.key_count(), keys.len() as i64);
    assert_eq!(index.delimiter(), Some(b':'));
    let state = index.scan_state().unwrap();
    assert_eq!(state.status, ScanStatus::Complete);
    assert_eq!(state.cursor, 0);

    let top = index.children(b"", None, 10, Sort::Count).unwrap();
    let names: Vec<String> = top.iter().map(|n| String::from_utf8_lossy(&n.segment).into_owned()).collect();
    assert_eq!(names, vec!["user", "event", "order"]);
    assert_eq!(top[0].count, 10_000);
    assert_eq!(top[0].children, 5_000);
    assert_eq!(top[0].classes.numeric, 5_000);
    assert_eq!(top[2].classes.hex, 5_000);
    let u = index.node(b"user:", b"7").unwrap().unwrap();
    assert_eq!(u.count, 2);
    assert!(!u.leaf);
    let (k, _) = index.table_counts().unwrap();
    assert_eq!(k, keys.len() as i64);
}

#[tokio::test]
async fn rescan_prunes_deleted_keys_and_keeps_counts_exact() {
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(4_000);
    fill(&mut conn, &keys).await;

    let index = Index::open(Location::Memory, &IndexOptions::default()).unwrap();
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    let index = outcome.index;
    assert_eq!(index.key_count(), 4_000);

    // delete all carts and add new events
    let carts: Vec<&[u8]> = keys.iter().filter(|k| k.ends_with(":cart")).map(|k| k.as_bytes()).collect();
    conn.raw(&testing::del(&carts)).await.unwrap();
    let mut pipe = commands::pipeline();
    for i in 0..100 {
        pipe.add_command(testing::set(format!("event:new:{i}").as_bytes(), b"v"));
    }
    conn.pipeline(&pipe).await.unwrap();

    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    assert_eq!(outcome.progress.pruned, carts.len() as u64);
    assert_eq!(outcome.progress.indexed_new, 100);
    let index = outcome.index;
    assert_eq!(index.key_count(), 4_000 - 1_000 + 100);
    let user = index.node(b"", b"user").unwrap().unwrap();
    assert_eq!(user.count, 1_000);
    let u = index.node(b"user:", b"3").unwrap().unwrap();
    assert_eq!(u.count, 1);
    assert_eq!(u.children, 1);
    let event = index.node(b"", b"event").unwrap().unwrap();
    assert_eq!(event.count, 1_100);
    assert_eq!(event.children, 1_001);
    assert_eq!(event.classes.literal, 1);
}

#[tokio::test]
async fn match_scan_is_partial_and_never_prunes() {
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    fill(&mut conn, &sample_keys(2_000)).await;
    let index = Index::open(Location::Memory, &IndexOptions::default()).unwrap();
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    let config = ScanConfig {
        pattern: Some(b"order:*".to_vec()),
        ..ScanConfig::default()
    };
    let outcome = scan::start(manager.connect().await.unwrap(), outcome.index, config)
        .wait()
        .await
        .unwrap();
    assert!(outcome.progress.partial);
    assert_eq!(outcome.progress.pruned, 0);
    assert_eq!(outcome.progress.scanned, 500);
    assert_eq!(outcome.index.key_count(), 2_000);
}

#[tokio::test]
async fn stop_persists_cursor_and_resume_completes() {
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(30_000);
    fill(&mut conn, &keys).await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resume.sqlite");
    let index = Index::open(Location::File(path.clone()), &IndexOptions::default()).unwrap();
    // small COUNT and a polite gap so there is time to stop midway
    let config = ScanConfig {
        tuning: scan::CountTuning {
            initial: 500,
            min: 500,
            max: 500,
            ..Default::default()
        },
        gap: Some(Duration::from_millis(5)),
        ..ScanConfig::default()
    };
    let handle = scan::start(manager.connect().await.unwrap(), index, config);
    let mut progress = handle.progress();
    loop {
        progress.changed().await.unwrap();
        let p = progress.borrow().clone();
        if p.calls >= 5 {
            handle.pause();
            break;
        }
    }
    // paused: no new calls
    tokio::time::sleep(Duration::from_millis(100)).await;
    let calls_paused = progress.borrow().calls;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(progress.borrow().calls, calls_paused);
    assert_eq!(progress.borrow().phase, ScanPhase::Paused);
    handle.resume();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().calls >= calls_paused + 3 {
            handle.stop();
            break;
        }
    }
    let outcome = handle.wait().await.unwrap();
    assert_eq!(outcome.progress.phase, ScanPhase::Stopped);
    let partial_keys = outcome.index.key_count();
    assert!(partial_keys > 0 && partial_keys < keys.len() as i64, "{partial_keys}");
    let state = outcome.index.scan_state().unwrap();
    assert_eq!(state.status, ScanStatus::Running);
    assert_ne!(state.cursor, 0);
    drop(outcome);

    // reopen the file and resume
    let index = Index::open(Location::File(path), &IndexOptions::default()).unwrap();
    assert_eq!(index.key_count(), partial_keys);
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    assert!(outcome.progress.resumed);
    assert_eq!(outcome.progress.phase, ScanPhase::Complete);
    assert_eq!(outcome.progress.generation, state.generation);
    assert_eq!(outcome.index.key_count(), keys.len() as i64);
    assert_eq!(outcome.index.table_counts().unwrap().0, keys.len() as i64);
}

#[tokio::test]
async fn scan_without_dbsize_still_completes() {
    // DBSIZE is read through the same path as everything else; here we only
    // assert that an empty database yields a complete, empty generation.
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    conn.raw(&testing::flushdb()).await.unwrap();
    let index = Index::open(Location::Memory, &IndexOptions::default()).unwrap();
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    assert_eq!(outcome.progress.phase, ScanPhase::Complete);
    assert_eq!(outcome.progress.dbsize, Some(0));
    assert_eq!(outcome.index.key_count(), 0);
    assert_eq!(outcome.index.delimiter(), Some(b':'));
    assert!(matches!(outcome.index.children(b"", None, 10, Sort::Name).unwrap().as_slice(), []));
    match conn.raw(&commands::dbsize()).await.unwrap() {
        Value::Int(0) => {}
        other => panic!("{other:?}"),
    }
}

/// CONCEPT 7 "on-demand index": a sample seeds the tree and establishes
/// nothing; the scan of a subtree makes the index exact under its prefix
/// and leaves the rest, and the full scan's own state, alone.
#[tokio::test]
async fn subtree_scans_have_their_own_freshness() {
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(4000);
    fill(&mut conn, &keys).await;
    let count = |index: &Index, seg: &[u8]| index.node(b"", seg).unwrap().map(|n| n.count).unwrap_or(0);

    // a sample: stops early, completes, covers nothing
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let config = ScanConfig {
        sample: Some(300),
        tuning: scan::CountTuning {
            initial: 100,
            max: 100,
            ..scan::CountTuning::default()
        },
        ..ScanConfig::default()
    };
    let out = scan::start(manager.connect().await.unwrap(), index, config).wait().await.unwrap();
    let p = &out.progress;
    assert_eq!(p.phase, ScanPhase::Complete, "{p:?}");
    assert!(p.partial && p.sample == Some(300) && !p.is_estimate());
    assert!(p.scanned >= 300 && p.scanned < 2000, "{p:?}");
    assert_eq!(p.pruned, 0);
    let mut index = out.index;
    assert_eq!(index.key_count() as u64, p.indexed_new);
    assert!(index.key_count() >= 300 && index.key_count() < 2000);
    assert_eq!(index.delimiter(), Some(b':'));
    assert!(index.coverage(b"").unwrap().is_none());
    assert!(index.coverage(b"user:").unwrap().is_none());
    assert_eq!(
        index.scan_state().unwrap().status,
        ScanStatus::Idle,
        "a sample is not the full scan"
    );
    let sampled_orders = count(&index, b"order");
    assert!(sampled_orders > 0 && sampled_orders < 1000);
    // a stale entry the sample could not know about
    let g = index.next_generation().unwrap();
    index.add(vec![b"user:gone:profile".to_vec(), b"order:gone".to_vec()], g).unwrap();

    // the subtree: every key under user:, and the stale one goes
    let config = ScanConfig {
        subtree: Some(b"user:".to_vec()),
        ..ScanConfig::default()
    };
    let out = scan::start(manager.connect().await.unwrap(), index, config.clone())
        .wait()
        .await
        .unwrap();
    let p = &out.progress;
    assert_eq!(p.phase, ScanPhase::Complete, "{p:?}");
    assert_eq!(p.subtree.as_deref(), Some(&b"user:"[..]));
    assert!(p.partial && p.is_estimate());
    assert_eq!(p.fraction(), Some(1.0));
    assert!(p.examined >= 4000, "the server walked every key: {p:?}");
    assert_eq!((p.scanned, p.pruned), (2000, 1));
    let index = out.index;
    assert_eq!(count(&index, b"user"), 2000);
    assert_eq!(count(&index, b"order"), sampled_orders + 1, "outside the subtree nothing changes");
    let c = index.coverage(b"user:").unwrap().expect("covered");
    assert_eq!(c.by, b"user:");
    assert_eq!(index.coverage(b"user:7:").unwrap().unwrap().by, b"user:");
    assert!(index.coverage(b"order:").unwrap().is_none());
    assert!(index.coverage(b"").unwrap().is_none());
    assert_eq!(index.scan_state().unwrap().status, ScanStatus::Idle);

    // the server changes under user: and under order:; the subtree is scanned again
    let order = format!("order:{:032x}", 2);
    let gone: Vec<&[u8]> = vec![b"user:1:profile", b"user:1:cart", b"user:2:cart", order.as_bytes()];
    conn.raw(&testing::del(&gone)).await.unwrap();
    conn.raw(&testing::set(b"user:new:profile", b"v")).await.unwrap();
    let out = scan::start(manager.connect().await.unwrap(), index, config).wait().await.unwrap();
    let p = &out.progress;
    assert_eq!((p.scanned, p.pruned, p.indexed_new), (1998, 3, 1), "{p:?}");
    assert!(!p.resumed);
    let index = out.index;
    assert_eq!(count(&index, b"user"), 1998);
    assert_eq!(count(&index, b"order"), sampled_orders + 1);
    assert!(index.coverage(b"user:").unwrap().unwrap().generation > c.generation);
    assert_eq!(index.subtree_scans().unwrap().len(), 1);

    // an interrupted full scan keeps its cursor through a subtree scan
    let config = ScanConfig {
        tuning: scan::CountTuning {
            initial: 50,
            min: 50,
            max: 50,
            ..scan::CountTuning::default()
        },
        gap: Some(Duration::from_millis(5)),
        ..ScanConfig::default()
    };
    let handle = scan::start(manager.connect().await.unwrap(), index, config);
    let mut progress = handle.progress();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().calls >= 5 {
            handle.stop();
            break;
        }
    }
    let out = handle.wait().await.unwrap();
    assert_eq!(out.progress.phase, ScanPhase::Stopped);
    let full = out.progress.generation;
    let index = out.index;
    assert!(index.scan_state().unwrap().resumable());
    let config = ScanConfig {
        subtree: Some(b"event:".to_vec()),
        ..ScanConfig::default()
    };
    let out = scan::start(manager.connect().await.unwrap(), index, config).wait().await.unwrap();
    assert_eq!(out.progress.scanned, 1000);
    assert!(out.progress.generation > full);
    let index = out.index;
    let state = index.scan_state().unwrap();
    assert!(state.resumable() && state.generation == full, "{state:?}");
    let out = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    let p = &out.progress;
    assert!(p.resumed && p.generation == full && p.phase == ScanPhase::Complete, "{p:?}");
    let index = out.index;
    // the full scan pruned the stale order key and covers everything now
    assert_eq!(index.key_count(), dbsize(&mut conn).await);
    assert_eq!(count(&index, b"order"), 999);
    assert!(index.subtree_scans().unwrap().is_empty());
    assert!(index.coverage(b"order:").unwrap().unwrap().by.is_empty());

    // a subtree scan needs a prefix and nothing else
    let bad = ScanConfig {
        subtree: Some(Vec::new()),
        ..ScanConfig::default()
    };
    let fresh = || Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    assert!(scan::start(manager.connect().await.unwrap(), fresh(), bad).wait().await.is_err());
    let bad = ScanConfig {
        subtree: Some(b"user:".to_vec()),
        pattern: Some(b"x*".to_vec()),
        ..ScanConfig::default()
    };
    assert!(scan::start(manager.connect().await.unwrap(), fresh(), bad).wait().await.is_err());
}

/// CONCEPT 8 "command console": a typed line is split, judged by what
/// the server says the command is, run on the console's own connection,
/// and the keys it touched are read again so the index follows.
#[tokio::test]
async fn the_console_runs_typed_commands_and_the_index_follows() {
    use keyfold_core::console::{self, Action, Confirm, Console, Effect, Limits};
    use keyfold_core::mutate;
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    fill(&mut conn, &sample_keys(400)).await;
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let mut index = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap()
        .index;
    let mut console = Console::new(Router::single(manager.clone(), manager.connect().await.unwrap()));
    let text = |v: &Value| console::render(v, &Limits::default()).lines.join("\n");

    // a read: runs on any tag, changes nothing
    let args = console::split_line("GET user:1:profile").unwrap();
    let verdict = console.inspect(&args, true).await.unwrap();
    assert_eq!((&verdict.action, verdict.mutating), (&Action::Run, false));
    let ran = console.run(&args, None, &verdict).await.unwrap();
    assert_eq!(text(ran.reply.as_ref().unwrap()), "\"v\"");
    assert_eq!(ran.effect, Effect::None);

    // a write with a name that is not UTF-8: asks on prod, and names its key
    let args = console::split_line(r#"HSET "c:\xff:h" f "two words" g 2"#).unwrap();
    assert_eq!(args[1], b"c:\xff:h");
    assert_eq!(console.inspect(&args, true).await.unwrap().action, Action::Confirm(Confirm::Write));
    let verdict = console.inspect(&args, false).await.unwrap();
    assert_eq!(verdict.action, Action::Run);
    let ran = console.run(&args, None, &verdict).await.unwrap();
    assert_eq!(text(ran.reply.as_ref().unwrap()), "(integer) 2");
    assert_eq!(ran.effect, Effect::Keys(vec![b"c:\xff:h".to_vec()]));
    let Effect::Keys(keys) = &ran.effect else { unreachable!() };
    for o in console::reconcile(console.router(), keys).await.unwrap() {
        assert!(o.exists());
        mutate::write_through(&mut index, &o).unwrap();
    }
    assert_eq!(index.key_count(), 401);

    // several keys in one command, one of them gone afterwards, one never there
    let args = console::split_line("DEL user:1:profile user:1:cart nothing:here").unwrap();
    let verdict = console.inspect(&args, false).await.unwrap();
    let ran = console.run(&args, None, &verdict).await.unwrap();
    assert_eq!(text(ran.reply.as_ref().unwrap()), "(integer) 2");
    let Effect::Keys(keys) = &ran.effect else {
        panic!("{:?}", ran.effect)
    };
    assert_eq!(keys.len(), 3);
    for o in console::reconcile(console.router(), keys).await.unwrap() {
        assert!(!o.exists());
        mutate::write_through(&mut index, &o).unwrap();
    }
    assert_eq!(index.key_count(), 399);
    // a rename moves the index entry
    let args = console::split_line("RENAME user:2:cart moved:cart").unwrap();
    let verdict = console.inspect(&args, false).await.unwrap();
    let ran = console.run(&args, None, &verdict).await.unwrap();
    let Effect::Keys(keys) = &ran.effect else {
        panic!("{:?}", ran.effect)
    };
    for o in console::reconcile(console.router(), keys).await.unwrap() {
        mutate::write_through(&mut index, &o).unwrap();
    }
    assert_eq!(names_matching(&index, "moved:*"), vec!["moved:cart"]);
    assert!(names_matching(&index, "user:2:*") == vec!["user:2:profile"]);
    assert_eq!(index.key_count(), dbsize(&mut conn).await);

    // the server's error is the reply; nothing is assumed to have changed wrongly
    let args = console::split_line("LPUSH user:3:profile x").unwrap();
    let verdict = console.inspect(&args, false).await.unwrap();
    let ran = console.run(&args, None, &verdict).await.unwrap();
    let err = ran.reply.as_ref().unwrap_err();
    assert!(console::render_error(err).starts_with("(error) WRONGTYPE"), "{err}");
    // a reply in several shapes
    let args = console::split_line("HGETALL \"c:\\xff:h\"").unwrap();
    let verdict = console.inspect(&args, false).await.unwrap();
    let ran = console.run(&args, None, &verdict).await.unwrap();
    let shown = text(ran.reply.as_ref().unwrap());
    assert!(shown.contains("\"two words\"") && shown.lines().count() == 2, "{shown}");
    let args = console::split_line("INFO server").unwrap();
    let verdict = console.inspect(&args, true).await.unwrap();
    assert_eq!(verdict.action, Action::Run, "INFO is read-only whatever category it is in");
    let ran = console.run(&args, None, &verdict).await.unwrap();
    assert!(text(ran.reply.as_ref().unwrap()).lines().count() > 5);

    // what the console does not run, and what it asks for
    for line in [
        "SELECT 1",
        "MULTI",
        "SUBSCRIBE c",
        "BLPOP q 0",
        "CLIENT REPLY OFF",
        "XREAD BLOCK 0 STREAMS s $",
    ] {
        let args = console::split_line(line).unwrap();
        let verdict = console.inspect(&args, false).await.unwrap();
        assert!(matches!(verdict.action, Action::Refuse(_)), "{line}: {verdict:?}");
        assert!(console.run(&args, None, &verdict).await.is_err());
    }
    let ask = |line: &'static str| console::split_line(line).unwrap();
    assert!(matches!(
        console.inspect(&ask("KEYS *"), false).await.unwrap().action,
        Action::Confirm(Confirm::Danger(_))
    ));
    assert!(matches!(
        console.inspect(&ask("CONFIG SET slowlog-max-len 128"), false).await.unwrap().action,
        Action::Confirm(Confirm::Danger(_))
    ));
    assert_eq!(
        console.inspect(&ask("CONFIG GET maxmemory"), true).await.unwrap().action,
        Action::Run
    );
    let verdict = console.inspect(&ask("FLUSHDB"), false).await.unwrap();
    assert!(matches!(verdict.action, Action::Confirm(Confirm::Typed(_))));
    // confirmed: it runs, and says the database is empty
    let ran = console.run(&ask("FLUSHDB"), None, &verdict).await.unwrap();
    assert_eq!(ran.effect, Effect::Flushed);
    assert_eq!(dbsize(&mut conn).await, 0);
    // a script that names no keys may have changed anything
    let script = ask("EVAL \"return redis.call('SET', 'made:by:script', '1')\" 0");
    let verdict = console.inspect(&script, false).await.unwrap();
    assert!(verdict.writes_keys);
    assert_eq!(console.run(&script, None, &verdict).await.unwrap().effect, Effect::Unknown);
    // the browser's connection was never part of any of it
    assert_eq!(dbsize(&mut conn).await, 1);
}

/// A user whose ACL allows little: a denied command is the reply, and
/// without `COMMAND` the console still judges by name and counts the rest
/// as writes.
#[tokio::test]
async fn the_console_works_under_a_restricted_acl() {
    use keyfold_core::console::{self, Action, Confirm, Console, Effect};
    let Some((url, _guard)) = test_url().await else { return };
    let (_manager, mut conn) = connect(&url).await;
    conn.raw(&testing::flushdb()).await.unwrap();
    conn.raw(&testing::set(b"acl:k", b"v")).await.unwrap();
    conn.raw(&testing::acl_setuser(
        "keyfold-test-reader",
        &[
            "reset",
            "on",
            ">secret",
            "~*",
            "+get",
            "+ping",
            "+select",
            "+hello",
            "+client|setinfo",
            "+info",
        ],
    ))
    .await
    .unwrap();

    let mut profile = Profile::from_url(&url).unwrap();
    profile.username = Some("keyfold-test-reader".into());
    profile.password = Some("secret".into());
    let manager = ConnectionManager::new(profile);
    let outcome = async {
        let mut console = Console::new(Router::single(manager.clone(), manager.connect().await?));
        // COMMAND INFO is denied: read by name it is unknown, so it counts as a write
        let args = console::split_line("GET acl:k").unwrap();
        let verdict = console.inspect(&args, true).await?;
        assert_eq!(verdict.action, Action::Confirm(Confirm::Write), "{verdict:?}");
        let verdict = console.inspect(&args, false).await?;
        assert_eq!(verdict.action, Action::Run);
        // COMMAND GETKEYS is denied too: it runs, and what it touched is unknown
        let ran = console.run(&args, None, &verdict).await?;
        assert!(matches!(ran.reply, Ok(Value::BulkString(ref b)) if b == b"v"), "{:?}", ran.reply);
        assert_eq!(ran.effect, Effect::Unknown);
        // a command the ACL does not grant: the denial is the reply
        let args = console::split_line("SET acl:k w").unwrap();
        let verdict = console.inspect(&args, false).await?;
        let ran = console.run(&args, None, &verdict).await?;
        let err = ran.reply.unwrap_err();
        assert!(err.is_denied(), "{err:?}");
        assert!(console::render_error(&err).contains("NOPERM"));
        assert_eq!(ran.effect, Effect::None);
        // refusals do not depend on the server's description
        let verdict = console.inspect(&console::split_line("SELECT 3").unwrap(), false).await?;
        assert!(matches!(verdict.action, Action::Refuse(_)));
        keyfold_core::Result::Ok(())
    }
    .await;
    conn.raw(&testing::acl_deluser("keyfold-test-reader")).await.unwrap();
    outcome.unwrap();
    match conn.raw(&commands::dbsize()).await.unwrap() {
        Value::Int(1) => {}
        other => panic!("{other:?}"),
    }
}

/// CONCEPT 7 "no index": plain SCAN pages reach every key, a page never
/// holds more than it was asked for, and a pattern that matches little
/// still comes to an end.
#[tokio::test]
async fn pages_without_an_index_walk_every_key_once() {
    use keyfold_core::browse::{PageOptions, Pager};
    use keyfold_core::scan::ScanNode;
    use std::collections::HashSet;
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(3000);
    fill(&mut conn, &keys).await;
    let expected: HashSet<Vec<u8>> = keys.iter().map(|k| k.clone().into_bytes()).collect();

    let node = || async { vec![ScanNode::single(manager.connect().await.unwrap(), keyfold_core::Role::Primary)] };
    let opts = PageOptions::default();
    let mut pager = Pager::new(node().await, None, None);
    let mut all: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;
    loop {
        let p = pager.next_page(&opts).await.unwrap();
        assert!(p.keys.len() <= 200, "a page of {} keys", p.keys.len());
        pages += 1;
        all.extend(p.keys);
        if p.done {
            break;
        }
        assert!(pages < 200, "the walk does not end");
    }
    assert!(pages >= 15, "3000 keys in {pages} pages");
    assert!(pager.is_done());
    assert_eq!(pager.returned(), all.len() as u64);
    // the database does not change during the walk, so nothing repeats
    assert_eq!(all.len(), 3000);
    assert_eq!(all.into_iter().collect::<HashSet<_>>(), expected);
    // a finished walk stays finished
    let p = pager.next_page(&opts).await.unwrap();
    assert!(p.keys.is_empty() && p.done && p.calls == 0, "{p:?}");

    // a pattern: only matches cross the wire, in pages of the same size
    let mut pager = Pager::new(node().await, Some(b"user:*:cart".to_vec()), None);
    let small = PageOptions { want: 100, ..opts };
    let mut carts = HashSet::new();
    loop {
        let p = pager.next_page(&small).await.unwrap();
        assert!(p.keys.len() <= 100);
        carts.extend(p.keys);
        if p.done {
            break;
        }
    }
    assert_eq!(carts.len(), 750);
    assert!(carts.iter().all(|k| k.ends_with(b":cart")));

    // a page is bounded by calls: one call of COUNT 10 over 3000 keys
    // cannot be the end, whatever it returned
    let mut pager = Pager::new(node().await, Some(b"event:2999".to_vec()), None);
    let tight = PageOptions {
        filter_count: 10,
        max_calls: 1,
        ..opts
    };
    let p = pager.next_page(&tight).await.unwrap();
    assert!(p.calls == 1 && !p.done && p.keys.len() <= 1, "{p:?}");
    let mut found = p.keys;
    let mut pages = 1;
    while !pager.is_done() {
        found.extend(pager.next_page(&tight).await.unwrap().keys);
        pages += 1;
        assert!(pages < 3000, "the walk does not end");
    }
    assert_eq!(found, vec![b"event:2999".to_vec()]);
    assert_eq!(pager.calls(), pages);

    // a type filter, and a walk over nothing
    let mut pager = Pager::new(node().await, None, Some("hash".into()));
    let mut none = 0;
    while !pager.is_done() {
        none += pager.next_page(&opts).await.unwrap().keys.len();
    }
    assert_eq!(none, 0);
    let p = Pager::new(Vec::new(), None, None).next_page(&opts).await.unwrap();
    assert!(p.done && p.keys.is_empty());
}

#[tokio::test]
async fn key_meta_is_pipelined_and_tolerant() {
    let Some((url, _guard)) = test_url().await else { return };
    let (_m, mut conn) = connect(&url).await;
    conn.raw(&testing::flushdb()).await.unwrap();
    conn.raw(&testing::set(b"m:1", b"hello")).await.unwrap();
    let mut expire = redis::cmd("EXPIRE");
    expire.arg("m:1").arg(1000);
    conn.raw(&expire).await.unwrap();
    let metas = keyfold_core::meta::fetch(&mut conn, &[b"m:1", b"m:missing"]).await.unwrap();
    assert_eq!(metas.len(), 2);
    assert!(metas[0].exists);
    assert_eq!(metas[0].key_type.as_deref(), Some("string"));
    assert!(metas[0].ttl.is_some_and(|t| t > 900));
    assert!(metas[0].mem.is_some_and(|m| m > 0));
    assert!(!metas[1].exists);
    assert_eq!(metas[1], keyfold_core::meta::KeyMeta::default());
    assert!(keyfold_core::meta::fetch(&mut conn, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn value_inspect_and_pages_cover_every_native_type() {
    use keyfold_core::value::{self, Cursor};
    let Some((url, _guard)) = test_url().await else { return };
    let (_m, mut conn) = connect(&url).await;
    conn.raw(&testing::flushdb()).await.unwrap();
    let mut pipe = commands::pipeline();
    let big = "x".repeat(70_000);
    pipe.add_command(testing::set(b"v:str", big.as_bytes()));
    pipe.add_command(testing::set(b"v:json", b"{\"a\":[1,2,3]}"));
    for i in 0..450 {
        let mut c = redis::cmd("HSET");
        c.arg("v:hash").arg(format!("f{i}")).arg(i);
        pipe.add_command(c);
        let mut c = redis::cmd("SADD");
        c.arg("v:set").arg(format!("m{i}"));
        pipe.add_command(c);
        let mut c = redis::cmd("ZADD");
        c.arg("v:zset").arg(i as f64 / 2.0).arg(format!("z{i}"));
        pipe.add_command(c);
        let mut c = redis::cmd("RPUSH");
        c.arg("v:list").arg(format!("i{i}"));
        pipe.add_command(c);
        let mut c = redis::cmd("XADD");
        c.arg("v:stream").arg("*").arg("k").arg(i).arg("w").arg("v");
        pipe.add_command(c);
    }
    for r in conn.pipeline(&pipe).await.unwrap() {
        r.unwrap();
    }

    let s = value::inspect(&mut conn, b"v:str").await.unwrap();
    assert!(s.exists);
    assert_eq!(s.key_type, "string");
    assert_eq!(s.length, Some(70_000));
    assert!(s.encoding.is_some());
    assert!(s.mem.is_some_and(|m| m > 70_000));
    let preview = value::string_chunk(&mut conn, b"v:str", 0, value::PREVIEW_BYTES).await.unwrap();
    assert_eq!(preview.len(), 65_536);
    assert_eq!(value::detect(&preview, false), value::Kind::Text);
    let rest = value::string_chunk(&mut conn, b"v:str", 65_536, value::CHUNK_BYTES).await.unwrap();
    assert_eq!(rest.len(), 70_000 - 65_536);
    let j = value::string_chunk(&mut conn, b"v:json", 0, 100).await.unwrap();
    assert_eq!(value::detect(&j, true), value::Kind::Json);

    for (key, ty) in [
        ("v:hash", "hash"),
        ("v:set", "set"),
        ("v:zset", "zset"),
        ("v:list", "list"),
        ("v:stream", "stream"),
    ] {
        let info = value::inspect(&mut conn, key.as_bytes()).await.unwrap();
        assert_eq!(info.key_type, ty, "{key}");
        assert_eq!(info.length, Some(450), "{key}");
        // walk every page
        let mut cursor = Cursor::Start;
        let mut total = 0;
        for _ in 0..100 {
            let p = value::page(&mut conn, key.as_bytes(), ty, cursor.clone(), 200, None).await.unwrap();
            total += p.entries.len();
            if p.done {
                break;
            }
            cursor = match ty {
                "hash" | "set" | "zset" => Cursor::Scan(p.cursor),
                "list" => Cursor::Rank(p.next.unwrap().parse().unwrap()),
                _ => Cursor::StreamId(p.next.unwrap()),
            };
        }
        assert_eq!(total, 450, "{key}");
    }
    // zset by rank carries scores; hash filter uses MATCH
    let ranked = value::page(&mut conn, b"v:zset", "zset", Cursor::Rank(0), 3, None).await.unwrap();
    assert_eq!(ranked.entries.len(), 3);
    assert_eq!(ranked.entries[0].score, Some(0.0));
    assert_eq!(ranked.entries[1].field, b"z1");
    assert_eq!(ranked.next.as_deref(), Some("3"));
    let filtered = value::page(&mut conn, b"v:hash", "hash", Cursor::Start, 200, Some(b"f1*"))
        .await
        .unwrap();
    assert!(filtered.entries.iter().all(|e| e.field.starts_with(b"f1")));
    let gone = value::inspect(&mut conn, b"v:missing").await.unwrap();
    assert!(!gone.exists);
    assert_eq!(gone.length, None);
    let (n, peek) = value::dump_peek(&mut conn, b"v:json", 4).await.unwrap();
    assert!(n > 4);
    assert_eq!(peek.len(), 4);
}

/// A glob verified against the server adds keys the index missed and
/// removes keys the server no longer has, without touching other keys.
/// Key names are byte strings (CONCEPT 3.1): names that are not UTF-8
/// are scanned, shown, opened, changed and deleted by their exact bytes,
/// and a delete under a prefix treats glob characters in it as bytes.
#[tokio::test]
async fn names_that_are_not_utf8_are_read_and_written_by_their_bytes() {
    use keyfold_core::meta;
    use keyfold_core::mutate::{self, DeleteConfig, DeletePhase, Op};
    use keyfold_core::search::{self, Plan, Query};
    use keyfold_core::value::{self, Cursor};
    use std::sync::atomic::AtomicBool;
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    conn.raw(&testing::flushdb()).await.unwrap();

    let never = AtomicBool::new(false);
    let a: &[u8] = b"bin:\xff\xfe";
    let b: &[u8] = b"bin:\x00\x01";
    let c: &[u8] = b"bin:\xff\xfe:child\x80";
    let h: &[u8] = b"bin:h\xc3";
    let texts: [&[u8]; 4] = [b"a*b:1", b"a*b:2", b"aXb:1", b"aXXb:1"];
    for k in [a, b, c].into_iter().chain(texts) {
        conn.raw(&testing::set(k, b"v")).await.unwrap();
    }
    for (field, value) in [(&b"f\xff"[..], &b"1"[..]), (b"plain", b"2")] {
        let op = Op::HashSet {
            field: field.to_vec(),
            value: value.to_vec(),
        };
        mutate::apply(&mut conn, h, &op, true).await.unwrap();
    }

    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let mut index = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap()
        .index;
    assert_eq!(index.key_count(), 8);
    let under = |index: &Index, prefix: &[u8]| -> Vec<Vec<u8>> {
        let plan = Plan::new(Query::prefix(prefix)).unwrap();
        index
            .search(&plan, None, 200, &never)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.name)
            .collect()
    };
    assert_eq!(under(&index, b"bin:"), vec![b.to_vec(), h.to_vec(), a.to_vec(), c.to_vec()]);
    // the tree splits on the delimiter byte only; a node can be a key and a parent
    let node = index.node(b"bin:", b"\xff\xfe").unwrap().expect("node");
    assert!(node.leaf && node.children == 1, "{node:?}");

    // reads by the exact bytes; the lossy rendering of the name is another key
    let info = value::inspect(&mut conn, a).await.unwrap();
    assert!(info.exists && info.key_type == "string", "{info:?}");
    let lossy = String::from_utf8_lossy(a).into_owned();
    assert!(!value::inspect(&mut conn, lossy.as_bytes()).await.unwrap().exists);
    let metas = meta::fetch(&mut conn, &[a, b, c, h, lossy.as_bytes()]).await.unwrap();
    assert_eq!(metas.iter().map(|m| m.exists).collect::<Vec<_>>(), [true, true, true, true, false]);
    assert_eq!(metas[3].key_type.as_deref(), Some("hash"));
    let page = value::page(&mut conn, h, "hash", Cursor::Start, 200, None).await.unwrap();
    let mut fields: Vec<&[u8]> = page.entries.iter().map(|e| &e.field[..]).collect();
    fields.sort();
    assert_eq!(fields, [&b"f\xff"[..], b"plain"]);

    // writes: expiry, a field addressed by its bytes, rename, delete
    let o = mutate::apply(&mut conn, a, &Op::Expire { seconds: 600 }, true).await.unwrap();
    assert!(o.ttl.is_some_and(|t| t > 500), "{o:?}");
    assert_eq!(o.added, vec![a.to_vec()]);
    mutate::write_through(&mut index, &o).unwrap();
    assert_eq!(index.key_count(), 8);

    let o = mutate::apply(&mut conn, h, &Op::HashDel { field: b"f\xff".to_vec() }, true)
        .await
        .unwrap();
    assert!(o.exists(), "{o:?}");
    let page = value::page(&mut conn, h, "hash", Cursor::Start, 200, None).await.unwrap();
    assert_eq!(page.entries.iter().map(|e| &e.field[..]).collect::<Vec<_>>(), [b"plain"]);

    let o = mutate::apply(&mut conn, b, &Op::Rename { to: b"bin:text".to_vec() }, true)
        .await
        .unwrap();
    assert_eq!((&o.removed, &o.added), (&vec![b.to_vec()], &vec![b"bin:text".to_vec()]));
    mutate::write_through(&mut index, &o).unwrap();
    assert_eq!(
        under(&index, b"bin:"),
        vec![h.to_vec(), b"bin:text".to_vec(), a.to_vec(), c.to_vec()]
    );

    let o = mutate::apply(&mut conn, a, &Op::Delete, true).await.unwrap();
    assert_eq!(o.removed, vec![a.to_vec()]);
    mutate::write_through(&mut index, &o).unwrap();
    assert!(!value::inspect(&mut conn, a).await.unwrap().exists);
    assert_eq!(index.key_count(), 7);
    // the child keeps its parent node, which is no longer a key
    let node = index.node(b"bin:", b"\xff\xfe").unwrap().expect("node");
    assert!(!node.leaf && node.children == 1, "{node:?}");

    // a delete under a prefix takes exactly the keys that start with it:
    // the typed pattern `a*b:*` matches four keys, the prefix `a*b:` two
    let typed = Plan::new(search::parse("a*b:*")).unwrap();
    assert_eq!(index.search_count(&typed, &never).unwrap(), Some(4));
    let plan = Plan::new(Query::prefix(b"a*b:")).unwrap();
    let handle = mutate::start_delete(
        Router::single(manager.clone(), manager.connect().await.unwrap()),
        index,
        plan,
        DeleteConfig::default(),
    );
    let out = handle.wait().await.unwrap();
    assert_eq!(out.progress.phase, DeletePhase::Complete, "{:?}", out.progress);
    assert_eq!((out.progress.total, out.progress.deleted, out.progress.removed), (Some(2), 2, 2));
    let index = out.index;
    assert_eq!(names_matching(&index, "a*"), vec!["aXXb:1", "aXb:1"]);
    assert_eq!(dbsize(&mut conn).await, 5);

    // and under a prefix that is not UTF-8
    let plan = Plan::new(Query::prefix(b"bin:\xff")).unwrap();
    let handle = mutate::start_delete(
        Router::single(manager.clone(), manager.connect().await.unwrap()),
        index,
        plan,
        DeleteConfig::default(),
    );
    let out = handle.wait().await.unwrap();
    assert_eq!((out.progress.total, out.progress.deleted), (Some(1), 1), "{:?}", out.progress);
    assert_eq!(under(&out.index, b"bin:"), vec![h.to_vec(), b"bin:text".to_vec()]);
    assert_eq!(dbsize(&mut conn).await, 4);
}

#[tokio::test]
async fn verify_reconciles_a_pattern_against_the_server() {
    use keyfold_core::search::{self, Plan, VerifyOptions};
    use std::sync::atomic::AtomicBool;
    let Some((url, _guard)) = test_url().await else { return };
    let (_m, mut conn) = connect(&url).await;
    let keys = sample_keys(2000);
    fill(&mut conn, &keys).await;
    let mut index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    index.set_delimiter(b':').unwrap();
    let g = index.begin_generation(false, None).unwrap();
    // the index is stale: one user key never scanned, one deleted on the server
    let mut stale: Vec<Vec<u8>> = keys.iter().map(|k| k.as_bytes().to_vec()).collect();
    stale.retain(|k| k != b"user:7:cart");
    stale.push(b"user:9999:cart".to_vec());
    index.ingest(stale, g, 0, 0).unwrap();
    index.complete_generation(g).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = Plan::new(search::parse("user:*:cart")).unwrap();
    let before = index.search_count(&plan, &cancel).unwrap().unwrap();
    assert_eq!(before, 500);

    let mut router = Router::single(_m.clone(), _m.connect().await.unwrap());
    let report = search::verify(&mut router, &mut index, b"user:*:cart", &VerifyOptions::default(), &cancel)
        .await
        .unwrap();
    assert!(report.complete && !report.truncated, "{report:?}");
    assert_eq!(report.matched, 500);
    assert_eq!(report.added, 1);
    assert_eq!(report.removed, 1);
    assert_eq!(index.search_count(&plan, &cancel).unwrap(), Some(500));
    assert_eq!(index.key_count(), 2000);
    let page = index
        .search(&Plan::new(search::parse("user:7:*")).unwrap(), None, 10, &cancel)
        .unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(
        index
            .search(&Plan::new(search::parse("user:9999:*")).unwrap(), None, 10, &cancel)
            .unwrap()
            .rows
            .is_empty()
    );
    // a scan cursor persisted for a resume survives the reconciliation
    assert_eq!(index.scan_state().unwrap().cursor, 0);

    // a bounded walk never removes
    let limited = VerifyOptions {
        max_calls: 1,
        count: 10,
        ..VerifyOptions::default()
    };
    conn.raw(&testing::del(&[b"user:8:cart"])).await.unwrap();
    let report = search::verify(&mut router, &mut index, b"user:*:cart", &limited, &cancel)
        .await
        .unwrap();
    assert!(report.truncated && !report.complete);
    assert_eq!(report.removed, 0);
    assert_eq!(index.search_count(&plan, &cancel).unwrap(), Some(500));
    assert!(search::exists(&mut conn, b"user:7:cart").await.unwrap());
    assert!(!search::exists(&mut conn, b"user:8:cart").await.unwrap());
}

fn names_matching(index: &Index, query: &str) -> Vec<String> {
    use keyfold_core::search::{self, Plan};
    use std::sync::atomic::AtomicBool;
    let plan = Plan::new(search::parse(query)).unwrap();
    index
        .search(&plan, None, 200, &AtomicBool::new(false))
        .unwrap()
        .rows
        .iter()
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        .collect()
}

async fn dbsize(conn: &mut Conn) -> i64 {
    match conn.raw(&commands::dbsize()).await.unwrap() {
        Value::Int(n) => n,
        other => panic!("DBSIZE {other:?}"),
    }
}

#[tokio::test]
async fn mutations_write_through_the_index() {
    use keyfold_core::mutate::{self, Op};
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(400);
    fill(&mut conn, &keys).await;
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    let mut index = outcome.index;
    assert_eq!(index.key_count(), 400);

    // a new string appears in the index in the same action
    let o = mutate::apply(
        &mut conn,
        b"new:str",
        &Op::SetString {
            value: b"hello".to_vec(),
            keep_ttl: false,
        },
        true,
    )
    .await
    .unwrap();
    assert!(o.exists() && o.ttl.is_none(), "{o:?}");
    assert_eq!(o.reply, "OK");
    let change = mutate::write_through(&mut index, &o).unwrap();
    assert_eq!(change.added, 1);
    assert_eq!(index.key_count(), 401);
    assert_eq!(names_matching(&index, "new:*"), vec!["new:str"]);

    // expiry is cached on the row; an edit with KEEPTTL keeps it; persist clears it
    let o = mutate::apply(&mut conn, b"new:str", &Op::Expire { seconds: 600 }, true)
        .await
        .unwrap();
    assert!(o.ttl.is_some_and(|t| t > 500), "{o:?}");
    mutate::write_through(&mut index, &o).unwrap();
    let plan = keyfold_core::search::Plan::new(keyfold_core::search::parse("new:str")).unwrap();
    let row = &index
        .search(&plan, None, 1, &std::sync::atomic::AtomicBool::new(false))
        .unwrap()
        .rows[0];
    assert!(row.ttl.is_some_and(|t| t > 500), "{row:?}");
    let o = mutate::apply(
        &mut conn,
        b"new:str",
        &Op::SetString {
            value: b"v2".to_vec(),
            keep_ttl: true,
        },
        true,
    )
    .await
    .unwrap();
    assert!(o.ttl.is_some(), "{o:?}");
    let o = mutate::apply(&mut conn, b"new:str", &Op::Persist, true).await.unwrap();
    assert!(o.exists() && o.ttl.is_none());
    mutate::write_through(&mut index, &o).unwrap();
    let row = &index
        .search(&plan, None, 1, &std::sync::atomic::AtomicBool::new(false))
        .unwrap()
        .rows[0];
    assert_eq!(row.ttl, None);
    assert!(
        mutate::apply(&mut conn, b"new:str", &Op::Expire { seconds: 0 }, true)
            .await
            .is_err()
    );

    // rename moves the row
    let o = mutate::apply(&mut conn, b"new:str", &Op::Rename { to: b"new:str2".to_vec() }, true)
        .await
        .unwrap();
    assert_eq!(o.removed, vec![b"new:str".to_vec()]);
    assert_eq!(o.added, vec![b"new:str2".to_vec()]);
    let change = mutate::write_through(&mut index, &o).unwrap();
    assert_eq!((change.added, change.removed), (1, 1));
    assert_eq!(index.key_count(), 401);
    assert_eq!(names_matching(&index, "new:*"), vec!["new:str2"]);

    // a collection that loses its last member is gone from the index too
    let o = mutate::apply(
        &mut conn,
        b"new:hash",
        &Op::HashSet {
            field: b"f".to_vec(),
            value: b"1".to_vec(),
        },
        true,
    )
    .await
    .unwrap();
    assert!(o.exists());
    assert_eq!(mutate::write_through(&mut index, &o).unwrap().added, 1);
    let o = mutate::apply(&mut conn, b"new:hash", &Op::HashDel { field: b"f".to_vec() }, true)
        .await
        .unwrap();
    assert!(!o.exists());
    assert_eq!(o.removed, vec![b"new:hash".to_vec()]);
    assert_eq!(mutate::write_through(&mut index, &o).unwrap().removed, 1);
    assert_eq!(index.key_count(), 401);

    // every other native type: add, change, remove
    let o = mutate::apply(&mut conn, b"new:set", &Op::SetAdd { member: b"m".to_vec() }, true)
        .await
        .unwrap();
    mutate::write_through(&mut index, &o).unwrap();
    let o = mutate::apply(&mut conn, b"new:set", &Op::SetRem { member: b"m".to_vec() }, true)
        .await
        .unwrap();
    assert!(!o.exists());
    mutate::write_through(&mut index, &o).unwrap();
    let o = mutate::apply(
        &mut conn,
        b"new:zset",
        &Op::ZAdd {
            member: b"m".to_vec(),
            score: 1.5,
        },
        true,
    )
    .await
    .unwrap();
    mutate::write_through(&mut index, &o).unwrap();
    let o = mutate::apply(&mut conn, b"new:zset", &Op::ZRem { member: b"m".to_vec() }, true)
        .await
        .unwrap();
    assert!(!o.exists());
    mutate::write_through(&mut index, &o).unwrap();
    for (v, head) in [(b"b".to_vec(), false), (b"a".to_vec(), true), (b"c".to_vec(), false)] {
        let o = mutate::apply(&mut conn, b"new:list", &Op::ListPush { value: v, head }, true)
            .await
            .unwrap();
        mutate::write_through(&mut index, &o).unwrap();
    }
    mutate::apply(
        &mut conn,
        b"new:list",
        &Op::ListSet {
            index: 1,
            value: b"B".to_vec(),
        },
        true,
    )
    .await
    .unwrap();
    mutate::apply(&mut conn, b"new:list", &Op::ListRem { index: 0 }, true)
        .await
        .unwrap();
    let items = conn.raw(&commands::lrange(b"new:list", 0, -1)).await.unwrap();
    assert_eq!(
        items,
        Value::Array(vec![Value::BulkString(b"B".to_vec()), Value::BulkString(b"c".to_vec())])
    );
    assert!(
        mutate::apply(&mut conn, b"new:list", &Op::ListRem { index: 99 }, true)
            .await
            .is_err()
    );
    let o = mutate::apply(
        &mut conn,
        b"new:stream",
        &Op::StreamAdd {
            fields: vec![(b"f".to_vec(), b"v".to_vec())],
        },
        true,
    )
    .await
    .unwrap();
    let id = o.reply.clone();
    assert!(id.contains('-'), "{id}");
    mutate::write_through(&mut index, &o).unwrap();
    let o = mutate::apply(&mut conn, b"new:stream", &Op::StreamDel { id }, true).await.unwrap();
    assert!(o.exists(), "an empty stream still exists");
    assert_eq!(index.key_count(), 403);
    assert_eq!(index.key_count(), dbsize(&mut conn).await);

    // delete, with UNLINK and with the DEL fallback
    let o = mutate::apply(&mut conn, b"new:list", &Op::Delete, true).await.unwrap();
    assert_eq!(o.reply, "1");
    assert_eq!(mutate::write_through(&mut index, &o).unwrap().removed, 1);
    let o = mutate::apply(&mut conn, b"new:stream", &Op::Delete, false).await.unwrap();
    assert_eq!(mutate::write_through(&mut index, &o).unwrap().removed, 1);
    let o = mutate::apply(&mut conn, b"never:existed", &Op::Delete, true).await.unwrap();
    assert_eq!(o.reply, "0");
    assert_eq!(mutate::write_through(&mut index, &o).unwrap().removed, 0);
    assert_eq!(index.key_count(), 401);
    assert_eq!(index.key_count(), dbsize(&mut conn).await);
    // a scan cursor persisted for a resume survives write-through
    assert_eq!(index.scan_state().unwrap().cursor, 0);
}

#[tokio::test]
async fn pattern_delete_runs_in_batches_with_pause_and_cancel() {
    use keyfold_core::mutate::{self, DeleteConfig, DeletePhase};
    use keyfold_core::search::{self, Plan};
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let keys = sample_keys(20_000);
    fill(&mut conn, &keys).await;
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    let index = outcome.index;
    assert_eq!(index.key_count(), 20_000);

    // a full run
    let plan = Plan::new(search::parse("user:*:cart")).unwrap();
    let handle = mutate::start_delete(
        Router::single(manager.clone(), manager.connect().await.unwrap()),
        index,
        plan.clone(),
        DeleteConfig {
            batch: 700,
            ..DeleteConfig::default()
        },
    );
    let out = handle.wait().await.unwrap();
    let p = &out.progress;
    assert_eq!(p.phase, DeletePhase::Complete, "{p:?}");
    assert_eq!(p.total, Some(5000));
    assert_eq!((p.matched, p.deleted, p.removed), (5000, 5000, 5000));
    assert_eq!(p.calls, 8);
    let index = out.index;
    assert_eq!(index.key_count(), 15_000);
    assert_eq!(dbsize(&mut conn).await, 15_000);
    assert_eq!(
        index.search_count(&plan, &std::sync::atomic::AtomicBool::new(false)).unwrap(),
        Some(0)
    );
    assert_eq!(names_matching(&index, "user:7:*"), vec!["user:7:profile"]);

    // cancel mid-way: what was deleted is gone from both, nothing else is
    let plan = Plan::new(search::parse("event:*")).unwrap();
    let handle = mutate::start_delete(
        Router::single(manager.clone(), manager.connect().await.unwrap()),
        index,
        plan.clone(),
        DeleteConfig {
            batch: 100,
            gap: Duration::from_millis(20),
            ..DeleteConfig::default()
        },
    );
    let mut progress = handle.progress();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().deleted > 0 {
            break;
        }
    }
    handle.cancel();
    let out = handle.wait().await.unwrap();
    let p = &out.progress;
    assert_eq!(p.phase, DeletePhase::Cancelled, "{p:?}");
    assert!(p.deleted > 0 && p.deleted < 5000, "{p:?}");
    assert_eq!(p.deleted, p.removed);
    let index = out.index;
    assert_eq!(index.key_count(), dbsize(&mut conn).await);
    let left = 5000 - p.deleted as i64;
    assert_eq!(
        index.search_count(&plan, &std::sync::atomic::AtomicBool::new(false)).unwrap(),
        Some(left)
    );

    // pause holds the server still, resume finishes; the DEL fallback works
    let handle = mutate::start_delete(
        Router::single(manager.clone(), manager.connect().await.unwrap()),
        index,
        plan.clone(),
        DeleteConfig {
            batch: 100,
            gap: Duration::from_millis(5),
            use_unlink: false,
        },
    );
    let mut progress = handle.progress();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().deleted > 0 {
            break;
        }
    }
    handle.pause();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().phase == DeletePhase::Paused {
            break;
        }
    }
    let held = dbsize(&mut conn).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(dbsize(&mut conn).await, held);
    handle.resume();
    let out = handle.wait().await.unwrap();
    assert_eq!(out.progress.phase, DeletePhase::Complete, "{:?}", out.progress);
    assert_eq!(out.progress.total, Some(left));
    assert_eq!(out.progress.deleted as i64, left);
    let index = out.index;
    assert_eq!(index.key_count(), 10_000);
    assert_eq!(dbsize(&mut conn).await, 10_000);

    // a flush clears the server; the caller clears the index
    let mut index = index;
    mutate::flush(&mut conn, false).await.unwrap();
    index.clear().unwrap();
    assert_eq!(dbsize(&mut conn).await, 0);
    assert_eq!(index.key_count(), 0);
}

/// CONCEPT 4, "prefer replicas": with a replica listed in `INFO
/// replication` the scan reads from it; the primary target reads the
/// primary; the polite profile refuses a primary unless asked for it.
/// Needs `KEYFOLD_TEST_REPLICA=1` (scripts/servers.sh starts one).
#[tokio::test]
async fn scans_prefer_a_reachable_replica() {
    use keyfold_core::scan::{ScanProfile, targets};
    use keyfold_core::{Role, ScanTarget};
    let Some((url, _guard)) = test_url().await else { return };
    if std::env::var("KEYFOLD_TEST_REPLICA").ok().as_deref() != Some("1") {
        eprintln!("KEYFOLD_TEST_REPLICA not set; skipping replica test");
        return;
    }
    let (manager, mut conn) = connect(&url).await;
    let caps = capability::probe(&mut conn, 15).await.unwrap();
    assert!(!caps.replicas.is_empty(), "the test primary should list a replica: {caps:?}");
    assert!(caps.replicas[0].online(), "{:?}", caps.replicas);
    let keys = sample_keys(5000);
    fill(&mut conn, &keys).await;
    // replication is asynchronous; wait until the replica has every key
    let replica = manager.connect_to(&caps.replicas[0].addr).await.unwrap();
    let mut replica = replica;
    for _ in 0..100 {
        if dbsize(&mut replica).await == 5000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let plan = targets(&manager, &caps, None, ScanTarget::Auto, ScanProfile::Fast).await.unwrap();
    assert_eq!(plan.nodes.len(), 1);
    assert_eq!(plan.nodes[0].role, Role::Replica);
    assert_eq!(plan.nodes[0].conn.addr(), &caps.replicas[0].addr);
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let outcome = scan::start_nodes(plan.nodes, index, ScanConfig::default()).wait().await.unwrap();
    assert_eq!(outcome.progress.phase, ScanPhase::Complete);
    assert_eq!(outcome.index.key_count(), 5000);
    assert_eq!(outcome.progress.nodes.len(), 1);
    assert_eq!(outcome.progress.nodes[0].role, Role::Replica);
    assert_eq!(outcome.progress.nodes[0].scanned, outcome.progress.scanned);

    let plan = targets(&manager, &caps, None, ScanTarget::Primary, ScanProfile::Polite)
        .await
        .unwrap();
    assert_eq!(plan.nodes[0].role, Role::Primary);
    assert_eq!(plan.nodes[0].conn.addr().port, manager.profile().port);
    let plan = targets(&manager, &caps, None, ScanTarget::Replica, ScanProfile::Normal)
        .await
        .unwrap();
    assert_eq!(plan.nodes[0].role, Role::Replica);

    // a server without replicas: the polite profile refuses, fast falls back
    let mut lonely = caps.clone();
    lonely.replicas.clear();
    let err = targets(&manager, &lonely, None, ScanTarget::Auto, ScanProfile::Polite)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("replicas only"), "{err}");
    assert!(
        targets(&manager, &lonely, None, ScanTarget::Replica, ScanProfile::Fast)
            .await
            .is_err()
    );
    let plan = targets(&manager, &lonely, None, ScanTarget::Auto, ScanProfile::Fast).await.unwrap();
    assert_eq!(plan.nodes[0].role, Role::Primary);
    assert_eq!(plan.notes, vec!["no replica listed; scanning the primary".to_string()]);
    // the explicit primary target overrides the polite rule
    let plan = targets(&manager, &lonely, None, ScanTarget::Primary, ScanProfile::Polite)
        .await
        .unwrap();
    assert_eq!(plan.nodes[0].role, Role::Primary);
    // an unreachable replica is noted and skipped
    let mut dead = caps.clone();
    dead.replicas[0].addr.port = 1;
    let plan = targets(&manager, &dead, None, ScanTarget::Auto, ScanProfile::Fast).await.unwrap();
    assert_eq!(plan.nodes[0].role, Role::Primary);
    assert_eq!(plan.notes.len(), 2, "{:?}", plan.notes);
    assert!(plan.notes[0].contains("unreachable"));
}

/// The normal profile samples `INFO stats` and `SLOWLOG LEN` between
/// calls; on an idle test server it never backs off and completes.
#[tokio::test]
async fn normal_profile_completes_with_load_sampling() {
    use keyfold_core::scan::ScanProfile;
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    fill(&mut conn, &sample_keys(3000)).await;
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let config = ScanConfig {
        profile: ScanProfile::Normal,
        tuning: keyfold_core::scan::CountTuning {
            initial: 100,
            max: 100,
            ..Default::default()
        },
        ..ScanConfig::default()
    };
    let outcome = scan::start(manager.connect().await.unwrap(), index, config).wait().await.unwrap();
    assert_eq!(outcome.progress.phase, ScanPhase::Complete);
    assert_eq!(outcome.index.key_count(), 3000);
    assert_eq!(outcome.progress.backoff, 0);
    // COUNT is a hint: a call returns whole hash buckets, so 3000 keys at
    // COUNT 100 take about 30 calls, not at least 30
    assert!(outcome.progress.calls >= 20, "{:?}", outcome.progress);
}

/// CONCEPT 7, keyspace notifications: with `notify-keyspace-events`
/// publishing key events, a subscription keeps the index live for new,
/// deleted, renamed and expired keys.
#[tokio::test]
async fn live_updates_follow_keyspace_notifications() {
    use keyfold_core::commands::write;
    use keyfold_core::mutate::write_through;
    use keyfold_core::notify::{self, LiveConfig, LivePhase};
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    fill(&mut conn, &sample_keys(1000)).await;
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let outcome = scan::start(manager.connect().await.unwrap(), index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    let mut index = outcome.index;

    let before = notify::setting(&mut conn).await.unwrap();
    let original = before.value.clone().expect("CONFIG GET allowed in tests");
    if before.keyevents_enabled() != Some(true) {
        let v = notify::enable(&mut conn).await.unwrap();
        assert!(notify::flags_cover_keyevents(&v), "{v}");
    }
    assert_eq!(notify::setting(&mut conn).await.unwrap().keyevents_enabled(), Some(true));

    let (handle, mut events) = notify::start(
        manager.clone(),
        vec![manager.profile().addr()],
        15,
        LiveConfig {
            flush_every: Duration::from_millis(50),
            ..LiveConfig::default()
        },
    );
    let mut status = handle.status();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            status.changed().await.unwrap();
            if status.borrow().phase == LivePhase::Live {
                break;
            }
        }
    })
    .await
    .expect("subscription went live");

    // new, deleted, renamed, expired
    let mut pipe = commands::pipeline();
    pipe.add_command(testing::set(b"live:new", b"1"));
    pipe.add_command(write::unlink(&[b"user:1:cart"]));
    pipe.add_command(write::rename(b"user:2:cart", b"live:renamed"));
    pipe.add_command(testing::set(b"live:short", b"1"));
    pipe.add_command(write::hset(b"live:hash", b"f", b"1"));
    for r in conn.pipeline(&pipe).await.unwrap() {
        r.unwrap();
    }
    let mut pexpire = redis::cmd("PEXPIRE");
    pexpire.arg("live:short").arg(50);
    conn.raw(&pexpire).await.unwrap();
    // read the expired key so the server notices (lazy expiry is enough)
    tokio::time::sleep(Duration::from_millis(120)).await;
    let _ = conn.raw(&commands::exists(&[b"live:short"])).await.unwrap();

    let mut seen: Vec<notify::Event> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(batch)) => {
                seen.extend(batch);
                let names: Vec<&str> = seen.iter().map(|e| e.event.as_str()).collect();
                if names.contains(&"expired") && names.contains(&"rename_to") && names.contains(&"hset") {
                    break;
                }
            }
            _ => break,
        }
    }
    let kinds: Vec<(&str, &str)> = seen
        .iter()
        .map(|e| (e.event.as_str(), std::str::from_utf8(&e.key).unwrap()))
        .collect();
    assert!(kinds.contains(&("set", "live:new")), "{kinds:?}");
    assert!(kinds.contains(&("del", "user:1:cart")), "{kinds:?}");
    assert!(kinds.contains(&("rename_from", "user:2:cart")), "{kinds:?}");
    assert!(kinds.contains(&("rename_to", "live:renamed")), "{kinds:?}");
    assert!(kinds.contains(&("expired", "live:short")), "{kinds:?}");
    assert!(kinds.contains(&("hset", "live:hash")), "{kinds:?}");

    for o in notify::reduce(&seen) {
        write_through(&mut index, &o).unwrap();
    }
    assert_eq!(names_matching(&index, "live:*"), vec!["live:hash", "live:new", "live:renamed"]);
    assert_eq!(names_matching(&index, "user:1:*"), vec!["user:1:profile"]);
    assert_eq!(names_matching(&index, "user:2:*"), vec!["user:2:profile"]);
    assert_eq!(index.key_count(), dbsize(&mut conn).await);
    assert!(status.borrow().events >= 6, "{:?}", *status.borrow());

    handle.stop();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if status.changed().await.is_err() || status.borrow().phase == LivePhase::Stopped {
                break;
            }
        }
    })
    .await
    .expect("subscription stopped");
    // leave the server as it was
    conn.raw(&commands::config_set(notify::PARAMETER, &original)).await.unwrap();
}

/// The header's database selector: key counts per non-empty database, the
/// configured number of databases, and a profile for another database of
/// the same server.
#[tokio::test]
async fn databases_are_listed_and_switchable() {
    let Some((url, _guard)) = test_url().await else { return };
    let (manager, mut conn) = connect(&url).await;
    let db = manager.profile().db;
    fill(&mut conn, &sample_keys(1200)).await;
    let caps = capability::probe(&mut conn, db).await.unwrap();
    let mut router = Router::new(manager.clone(), conn, &caps).await;

    let dbs = router.databases().await;
    assert!(dbs.configured, "CONFIG GET databases should answer on the test server");
    assert!(i64::from(dbs.count) > db, "{dbs:?}");
    assert_eq!(dbs.keys.get(&db), Some(&1200), "{dbs:?}");

    // another database of the same server: own id (own index), same server
    let other_db = if db == 0 { 1 } else { db - 1 };
    let other = manager.profile().with_db(other_db);
    assert_ne!(other.id, manager.profile().id);
    let other_manager = ConnectionManager::new(other);
    let mut other_conn = other_manager.connect().await.expect("connect to the other database");
    let other_caps = capability::probe(&mut other_conn, other_db).await.unwrap();
    assert_eq!(other_caps.db, other_db);
    let expected = dbs.keys.get(&other_db).copied().unwrap_or(0);
    assert_eq!(other_caps.dbsize, Some(expected), "DBSIZE of db {other_db} against INFO keyspace");

    // a database the server does not have is refused, not silently db 0
    let beyond = ConnectionManager::new(manager.profile().with_db(i64::from(dbs.count)));
    assert!(beyond.connect().await.is_err());
}
