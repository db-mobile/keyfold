//! Integration tests against a live cluster (CONCEPT 4 "Cluster", 10.1).
//! Set `KEYFOLD_TEST_CLUSTER_URL` to any node (for example
//! `redis://127.0.0.1:16400/0`); `scripts/servers.sh` starts one with
//! three primaries and three replicas. Database 0 is flushed.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use keyfold_core::cluster::{self, key_slot};
use keyfold_core::commands::{self, testing, write};
use keyfold_core::index::{Index, IndexOptions, Location};
use keyfold_core::route::Hop;
use keyfold_core::scan::{self, ScanConfig, ScanPhase, ScanProfile, targets};
use keyfold_core::{ConnectionManager, Profile, Role, Router, ScanTarget, capability, routed};
use redis::Value;

static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn cluster_url() -> Option<(String, tokio::sync::MutexGuard<'static, ()>)> {
    match std::env::var("KEYFOLD_TEST_CLUSTER_URL") {
        Ok(u) if !u.is_empty() => Some((u, DB_LOCK.lock().await)),
        _ => {
            eprintln!("KEYFOLD_TEST_CLUSTER_URL not set; skipping cluster test");
            None
        }
    }
}

async fn connect(url: &str) -> (ConnectionManager, Router, capability::Capabilities) {
    let profile = Profile::from_url(url).unwrap();
    let manager = ConnectionManager::new(profile);
    let mut conn = manager.connect().await.expect("connect");
    let caps = capability::probe(&mut conn, 0).await.unwrap();
    assert!(caps.is_cluster(), "{url} is not a cluster node: mode {}", caps.mode);
    let router = Router::new(manager.clone(), conn, &caps).await;
    assert!(router.is_cluster(), "{:?}", router.note());
    (manager, router, caps)
}

fn sample_keys(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 3 {
            0 => format!("user:{}:profile", i / 3),
            1 => format!("order:{:032x}", i),
            _ => format!("event:{i}"),
        })
        .collect()
}

/// Write keys through the router: one pipeline per owning node.
async fn fill(router: &mut Router, keys: &[String]) {
    router.flush(false).await.unwrap();
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes()).collect();
    for chunk in refs.chunks(5000) {
        for (addr, idxs) in router.group_by_node(chunk) {
            let mut pipe = commands::pipeline();
            for &i in &idxs {
                pipe.add_command(testing::set(chunk[i], b"v"));
            }
            for r in routed!(router, Hop::node(&addr), |c| c.pipeline(&pipe).await).unwrap() {
                r.unwrap();
            }
        }
    }
}

async fn wait_replicas_synced(router: &mut Router, manager: &ConnectionManager, expected: u64) {
    let t = router.topology().unwrap().clone();
    for shard in &t.shards {
        let Some(p) = shard.primary() else { continue };
        let want = match routed!(router, Hop::node(&p.addr), |c| c.raw(&commands::dbsize()).await).unwrap() {
            Value::Int(n) => n as u64,
            _ => 0,
        };
        for r in shard.replicas() {
            let mut c = manager.connect_to(&r.addr).await.unwrap();
            c.raw(&commands::readonly()).await.unwrap();
            for _ in 0..200 {
                if let Ok(Value::Int(n)) = c.raw(&commands::dbsize()).await
                    && n as u64 == want
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    assert_eq!(router.dbsize().await, Some(expected));
}

#[tokio::test]
async fn topology_and_slots_match_the_server() {
    let Some((url, _guard)) = cluster_url().await else { return };
    let (_manager, mut router, caps) = connect(&url).await;
    assert!(caps.has("cluster|shards") || caps.has("cluster|slots"));
    let t = router.topology().unwrap().clone();
    assert_eq!(t.shards.len(), 3, "{t:?}");
    assert_eq!(t.unassigned_slots(), 0);
    assert_eq!(t.primaries().count(), 3);
    assert!(t.node_count() >= 3);

    // the client-side slot equals CLUSTER KEYSLOT, and the slot's primary
    // accepts the key without a redirect
    for key in sample_keys(120)
        .iter()
        .chain([&"{tag}:a".to_string(), &"{tag}:b".to_string(), &"x{}y".to_string()])
    {
        let cmd = testing::cluster_keyslot(key.as_bytes());
        let seed = router.seed().clone();
        let v = routed!(router, Hop::node(&seed), |c| c.raw(&cmd).await).unwrap();
        assert_eq!(Value::Int(key_slot(key.as_bytes()) as i64), v, "slot of {key}");
        let addr = router.addr_for_key(key.as_bytes()).unwrap();
        let shard = t.shard_for_slot(key_slot(key.as_bytes())).unwrap();
        assert_eq!(shard.primary().unwrap().addr, addr);
    }
    assert_eq!(key_slot(b"{tag}:a"), key_slot(b"{tag}:b"));

    // CLUSTER SLOTS describes the same topology
    let seed = router.seed().clone();
    let slots = routed!(router, Hop::node(&seed), |c| c.raw(&commands::cluster_slots()).await).unwrap();
    let t2 = cluster::parse_slots(&slots, &seed.host).unwrap();
    for slot in [0u16, 5460, 5461, 10922, 10923, 16383] {
        assert_eq!(
            t.primary_for_slot(slot).map(|n| &n.addr),
            t2.primary_for_slot(slot).map(|n| &n.addr),
            "slot {slot}"
        );
    }
}

#[tokio::test]
async fn commands_follow_moved_redirects() {
    let Some((url, _guard)) = cluster_url().await else { return };
    let (_manager, mut router, _caps) = connect(&url).await;
    let t = router.topology().unwrap().clone();
    let key = b"redirect:me";
    let right = router.addr_for_key(key).unwrap();
    let wrong = t.primaries().map(|n| n.addr.clone()).find(|a| *a != right).unwrap();
    let set = testing::set(key, b"1");
    routed!(router, Hop::key(key), |c| c.raw(&set).await).unwrap();

    // asked at the wrong node directly, the server answers MOVED ...
    let exists = commands::exists(&[key]);
    let err = router.node(&wrong).await.unwrap().raw(&exists).await.unwrap_err();
    let r = err.redirect().expect("a MOVED reply");
    assert!(!r.ask);
    assert_eq!(r.slot, key_slot(key));
    assert_eq!(r.port, right.port);
    // ... and the routed form follows it
    let v = routed!(router, Hop::node(&wrong), |c| c.raw(&exists).await).unwrap();
    assert_eq!(v, Value::Int(1));

    // a stale slot map is corrected by the redirect
    let mut stale = Router::new(router.manager().clone(), router.manager().connect().await.unwrap(), &_caps).await;
    let mut wrong_topo = stale.topology().unwrap().clone();
    wrong_topo.moved(key_slot(key), &wrong);
    assert_eq!(wrong_topo.primary_for_key(key).unwrap().addr, wrong);
    stale.set_topology(wrong_topo);
    assert_eq!(stale.addr_for_key(key).unwrap(), wrong);
    let v = routed!(stale, Hop::key(key), |c| c.raw(&exists).await).unwrap();
    assert_eq!(v, Value::Int(1));
    assert_eq!(stale.topology().unwrap().primary_for_key(key).unwrap().addr, right);
    let del = write::unlink(&[key]);
    routed!(router, Hop::key(key), |c| c.raw(&del).await).unwrap();
}

#[tokio::test]
async fn cluster_scan_reads_one_node_per_shard() {
    let Some((url, _guard)) = cluster_url().await else { return };
    let (manager, mut router, caps) = connect(&url).await;
    let keys = sample_keys(30_000);
    fill(&mut router, &keys).await;
    wait_replicas_synced(&mut router, &manager, 30_000).await;
    let t = router.topology().unwrap().clone();
    let replicas_online = t.shards.iter().all(|s| s.replicas().any(|r| r.online));

    // replicas when they are all online, READONLY sent, one node per shard
    let plan = targets(&manager, &caps, Some(&t), ScanTarget::Auto, ScanProfile::Fast)
        .await
        .unwrap();
    assert_eq!(plan.nodes.len(), 3, "{:?}", plan.notes);
    if replicas_online {
        assert!(
            plan.nodes.iter().all(|n| n.role == Role::Replica),
            "{}",
            scan::describe(&plan.nodes)
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    }
    let ids: std::collections::BTreeSet<&str> = plan.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids.len(), 3);
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let outcome = scan::start_nodes(plan.nodes, index, ScanConfig::default()).wait().await.unwrap();
    let p = &outcome.progress;
    assert_eq!(p.phase, ScanPhase::Complete, "{p:?}");
    assert_eq!(p.dbsize, Some(30_000));
    assert_eq!(p.nodes.len(), 3);
    assert_eq!(p.scanned, p.nodes.iter().map(|n| n.scanned).sum::<u64>());
    assert!(
        p.nodes.iter().all(|n| n.done && n.scanned > 5000 && n.error.is_none()),
        "{:?}",
        p.nodes
    );
    let index = outcome.index;
    assert_eq!(index.key_count(), 30_000);
    // a complete generation leaves nothing to resume
    let state = index.scan_state().unwrap();
    assert!(!state.resumable(), "{state:?}");
    assert_eq!(state.cursor, 0);

    // the primary target reads the primaries
    let plan = targets(&manager, &caps, Some(&t), ScanTarget::Primary, ScanProfile::Fast)
        .await
        .unwrap();
    assert!(plan.nodes.iter().all(|n| n.role == Role::Primary));
    assert_eq!(plan.nodes.len(), 3);

    // stop after the first calls, then resume on the same nodes
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let config = ScanConfig {
        tuning: keyfold_core::scan::CountTuning {
            initial: 500,
            max: 500,
            ..Default::default()
        },
        gap: Some(Duration::from_millis(30)),
        ..ScanConfig::default()
    };
    let handle = scan::start_nodes(plan.nodes, index, config.clone());
    let mut progress = handle.progress();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().calls >= 3 {
            break;
        }
    }
    handle.stop();
    let out = handle.wait().await.unwrap();
    assert_eq!(out.progress.phase, ScanPhase::Stopped);
    let state = out.index.scan_state().unwrap();
    assert!(state.resumable(), "{state:?}");
    assert_eq!(state.cursors.len(), 3);
    assert_eq!(state.cursor, 1, "several cursors, some non-zero, read as 1");
    let plan = targets(&manager, &caps, Some(&t), ScanTarget::Primary, ScanProfile::Fast)
        .await
        .unwrap();
    let out = scan::start_nodes(plan.nodes, out.index, ScanConfig::default())
        .wait()
        .await
        .unwrap();
    assert!(out.progress.resumed, "{:?}", out.progress);
    assert_eq!(out.progress.phase, ScanPhase::Complete);
    assert_eq!(out.index.key_count(), 30_000);
    let (k, _) = out.index.table_counts().unwrap();
    assert_eq!(k, 30_000);

    // a different node set does not resume a stopped scan
    let plan_r = targets(&manager, &caps, Some(&t), ScanTarget::Auto, ScanProfile::Fast)
        .await
        .unwrap();
    if replicas_online {
        let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
        let plan_p = targets(&manager, &caps, Some(&t), ScanTarget::Primary, ScanProfile::Fast)
            .await
            .unwrap();
        let handle = scan::start_nodes(plan_p.nodes, index, config);
        let mut progress = handle.progress();
        loop {
            progress.changed().await.unwrap();
            if progress.borrow().calls >= 3 {
                break;
            }
        }
        handle.stop();
        let out = handle.wait().await.unwrap();
        let out = scan::start_nodes(plan_r.nodes, out.index, ScanConfig::default())
            .wait()
            .await
            .unwrap();
        assert!(!out.progress.resumed);
        assert_eq!(out.progress.phase, ScanPhase::Complete);
        assert_eq!(out.index.key_count(), 30_000);
    }
}

#[tokio::test]
async fn cluster_writes_deletes_and_verification_span_the_nodes() {
    use keyfold_core::meta;
    use keyfold_core::mutate::{self, DeleteConfig, DeletePhase, Op};
    use keyfold_core::search::{self, Plan, VerifyOptions};
    let Some((url, _guard)) = cluster_url().await else { return };
    let (manager, mut router, caps) = connect(&url).await;
    let keys = sample_keys(6000);
    fill(&mut router, &keys).await;
    let t = router.topology().unwrap().clone();
    let plan = targets(&manager, &caps, Some(&t), ScanTarget::Primary, ScanProfile::Fast)
        .await
        .unwrap();
    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let mut index = scan::start_nodes(plan.nodes, index, ScanConfig::default())
        .wait()
        .await
        .unwrap()
        .index;
    assert_eq!(index.key_count(), 6000);

    // single-key writes land on the owning primary
    for (i, k) in ["w:{a}", "w:{b}", "w:{c}", "w:{d}", "w:{e}"].iter().enumerate() {
        let op = Op::SetString {
            value: format!("{i}").into_bytes(),
            keep_ttl: false,
        };
        let outcome = routed!(router, Hop::key(k.as_bytes()), |c| mutate::apply(c, k.as_bytes(), &op, true).await).unwrap();
        assert!(outcome.exists());
        mutate::write_through(&mut index, &outcome).unwrap();
    }
    assert_eq!(index.key_count(), 6005);
    let owners: std::collections::BTreeSet<_> = ["w:{a}", "w:{b}", "w:{c}", "w:{d}", "w:{e}"]
        .iter()
        .map(|k| router.addr_for_key(k.as_bytes()).unwrap())
        .collect();
    assert!(owners.len() >= 2, "the sample keys should spread over the shards: {owners:?}");

    // metadata for a window is one pipeline per node, results in order
    let names: Vec<&[u8]> = keys[..200].iter().map(|k| k.as_bytes()).chain([&b"missing:key"[..]]).collect();
    let metas = meta::fetch_routed(&mut router, &names).await.unwrap();
    assert_eq!(metas.len(), 201);
    assert!(metas[..200].iter().all(|m| m.exists && m.key_type.as_deref() == Some("string")));
    assert!(!metas[200].exists);
    assert!(router.group_by_node(&names).len() >= 2);

    // a pattern delete: one pipeline of single-key UNLINKs per node and batch
    let plan = Plan::new(search::parse("order:*")).unwrap();
    let delete_router = Router::new(manager.clone(), manager.connect().await.unwrap(), &caps).await;
    let handle = mutate::start_delete(
        delete_router,
        index,
        plan.clone(),
        DeleteConfig {
            batch: 500,
            ..DeleteConfig::default()
        },
    );
    let out = handle.wait().await.unwrap();
    let p = &out.progress;
    assert_eq!(p.phase, DeletePhase::Complete, "{p:?}");
    assert_eq!((p.total, p.deleted, p.removed), (Some(2000), 2000, 2000));
    assert!(p.calls >= 4 && p.calls <= 12, "{p:?}");
    let mut index = out.index;
    assert_eq!(index.key_count(), 4005);
    assert_eq!(router.dbsize().await, Some(4005));
    let never = AtomicBool::new(false);
    assert_eq!(index.search_count(&plan, &never).unwrap(), Some(0));

    // verification walks every primary: a key added behind the index's
    // back on each shard is found, a stale entry removed
    let extra = ["user:x1:profile", "user:x2:profile", "user:x3:profile", "user:x4:profile"];
    for k in extra {
        let cmd = testing::set(k.as_bytes(), b"1");
        routed!(router, Hop::key(k.as_bytes()), |c| c.raw(&cmd).await).unwrap();
    }
    let g = index.scan_state().unwrap().generation;
    index.add(vec![b"user:stale:profile".to_vec()], g).unwrap();
    let report = search::verify(&mut router, &mut index, b"user:*:profile", &VerifyOptions::default(), &never)
        .await
        .unwrap();
    assert!(report.complete && !report.truncated, "{report:?}");
    assert_eq!(report.matched, 2004);
    assert_eq!(report.added, 4);
    assert_eq!(report.removed, 1);
    assert!(report.calls >= 3, "{report:?}");
    assert_eq!(index.key_count(), 4009);
    assert_eq!(router.dbsize().await, Some(4009));

    // flush reaches every primary
    router.flush(false).await.unwrap();
    assert_eq!(router.dbsize().await, Some(0));
}

/// The console on a cluster: a command with keys goes to the owner of
/// their slot, keys in different slots are caught before sending, and a
/// command without keys goes to the node that was chosen.
#[tokio::test]
async fn the_console_routes_typed_commands_by_their_keys() {
    use keyfold_core::console::{self, Action, Console, Effect};
    use keyfold_core::mutate;
    let Some((url, _guard)) = cluster_url().await else { return };
    let (manager, mut router, caps) = connect(&url).await;
    router.flush(false).await.unwrap();
    let mut console = Console::new(Router::new(manager.clone(), manager.connect().await.unwrap(), &caps).await);
    let mut index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    index.set_delimiter(b':').unwrap();

    let mut owners = std::collections::BTreeSet::new();
    for tag in ["a", "b", "c", "d", "e", "f"] {
        let line = format!("MSET {{{tag}}}:x 1 {{{tag}}}:y 2");
        let args = console::split_line(&line).unwrap();
        let verdict = console.inspect(&args, false).await.unwrap();
        assert_eq!(verdict.action, Action::Run);
        let ran = console.run(&args, None, &verdict).await.unwrap();
        assert_eq!(ran.reply.unwrap(), Value::Okay, "{line}");
        assert_eq!(ran.node, router.addr_for_key(format!("{{{tag}}}:x").as_bytes()).unwrap());
        owners.insert(ran.node.clone());
        let Effect::Keys(keys) = ran.effect else {
            panic!("no keys for {line}")
        };
        assert_eq!(keys.len(), 2);
        // read again through the browser's router, as the shell does
        for o in console::reconcile(&mut router, &keys).await.unwrap() {
            assert!(o.exists());
            mutate::write_through(&mut index, &o).unwrap();
        }
    }
    assert!(owners.len() >= 2, "the tags should spread over the shards: {owners:?}");
    assert_eq!(index.key_count(), 12);
    assert_eq!(router.dbsize().await, Some(12));

    // different slots: said before anything is sent
    let args = console::split_line("MSET {a}:x 9 {b}:x 9").unwrap();
    let verdict = console.inspect(&args, false).await.unwrap();
    let err = console.run(&args, None, &verdict).await.unwrap_err();
    assert!(err.to_string().contains("different hash slots"), "{err}");
    let args = console::split_line("GET {a}:x").unwrap();
    let verdict = console.inspect(&args, false).await.unwrap();
    assert_eq!(
        console.run(&args, None, &verdict).await.unwrap().reply.unwrap(),
        Value::BulkString(b"1".to_vec())
    );

    // without keys: the seed, or the node that was named
    let args = console::split_line("CLUSTER MYID").unwrap();
    let verdict = console.inspect(&args, true).await.unwrap();
    assert_eq!(verdict.action, Action::Run);
    let mut ids = std::collections::BTreeSet::new();
    let t = router.topology().unwrap().clone();
    for p in t.primaries() {
        let ran = console.run(&args, Some(&p.addr), &verdict).await.unwrap();
        assert_eq!(ran.node, p.addr);
        let Value::BulkString(id) = ran.reply.unwrap() else {
            panic!("CLUSTER MYID")
        };
        ids.insert(id);
    }
    assert_eq!(ids.len(), 3);
    let ran = console.run(&args, None, &verdict).await.unwrap();
    assert_eq!(&ran.node, router.seed());
    router.flush(false).await.unwrap();
}

/// A subtree scan on a cluster: every shard is walked with the pattern,
/// the subtree becomes exact, and its cursors are kept per node.
#[tokio::test]
async fn cluster_subtree_scan_covers_the_prefix_on_every_shard() {
    let Some((url, _guard)) = cluster_url().await else { return };
    let (manager, mut router, caps) = connect(&url).await;
    let keys = sample_keys(6000);
    fill(&mut router, &keys).await;
    wait_replicas_synced(&mut router, &manager, 6000).await;
    let t = router.topology().unwrap().clone();
    let count = |index: &Index, seg: &[u8]| index.node(b"", seg).unwrap().map(|n| n.count).unwrap_or(0);
    let plan = || async {
        targets(&manager, &caps, Some(&t), ScanTarget::Primary, ScanProfile::Fast)
            .await
            .unwrap()
            .nodes
    };

    let index = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
    let sample = ScanConfig {
        sample: Some(500),
        tuning: scan::CountTuning {
            initial: 100,
            max: 100,
            ..scan::CountTuning::default()
        },
        ..ScanConfig::default()
    };
    let out = scan::start_nodes(plan().await, index, sample).wait().await.unwrap();
    assert_eq!(out.progress.phase, ScanPhase::Complete, "{:?}", out.progress);
    assert!(out.progress.scanned >= 500 && out.progress.scanned < 6000, "{:?}", out.progress);
    let mut index = out.index;
    assert!(index.coverage(b"order:").unwrap().is_none());
    let g = index.next_generation().unwrap();
    index.add(vec![b"order:gone".to_vec()], g).unwrap();
    let sampled_users = count(&index, b"user");

    // stop the subtree scan midway, then resume it from the per-node cursors
    let slow = ScanConfig {
        subtree: Some(b"order:".to_vec()),
        tuning: scan::CountTuning {
            initial: 50,
            min: 50,
            max: 50,
            ..scan::CountTuning::default()
        },
        gap: Some(Duration::from_millis(5)),
        ..ScanConfig::default()
    };
    let handle = scan::start_nodes(plan().await, index, slow);
    let mut progress = handle.progress();
    loop {
        progress.changed().await.unwrap();
        if progress.borrow().calls >= 9 {
            handle.stop();
            break;
        }
    }
    let out = handle.wait().await.unwrap();
    assert_eq!(out.progress.phase, ScanPhase::Stopped, "{:?}", out.progress);
    let index = out.index;
    let run = index.subtree_run(b"order:").unwrap().expect("resumable");
    assert_eq!(run.cursors.len(), 3);
    assert!(
        index.coverage(b"order:").unwrap().is_none(),
        "an interrupted scan establishes nothing"
    );
    assert_eq!(index.subtree_scans().unwrap().iter().filter(|s| s.running).count(), 1);

    let config = ScanConfig {
        subtree: Some(b"order:".to_vec()),
        ..ScanConfig::default()
    };
    let out = scan::start_nodes(plan().await, index, config).wait().await.unwrap();
    let p = &out.progress;
    assert_eq!(p.phase, ScanPhase::Complete, "{p:?}");
    assert!(p.resumed && p.generation == run.generation, "{p:?}");
    assert_eq!(p.nodes.len(), 3);
    assert!(p.nodes.iter().all(|n| n.done && n.error.is_none()));
    assert_eq!(p.pruned, 1);
    let index = out.index;
    assert_eq!(count(&index, b"order"), 2000);
    assert_eq!(count(&index, b"user"), sampled_users);
    assert_eq!(index.coverage(b"order:").unwrap().unwrap().by, b"order:");
    assert!(index.coverage(b"user:").unwrap().is_none());
    assert!(index.subtree_run(b"order:").unwrap().is_none());
    router.flush(false).await.unwrap();
}

/// Paging without an index on a cluster: the nodes are read one after
/// another, a replica per shard when the target allows, and every key
/// comes by exactly once.
#[tokio::test]
async fn pages_without_an_index_walk_every_shard() {
    use keyfold_core::browse::{PageOptions, Pager};
    use std::collections::HashSet;
    let Some((url, _guard)) = cluster_url().await else { return };
    let (manager, mut router, caps) = connect(&url).await;
    let keys = sample_keys(3000);
    fill(&mut router, &keys).await;
    wait_replicas_synced(&mut router, &manager, 3000).await;
    let expected: HashSet<Vec<u8>> = keys.iter().map(|k| k.clone().into_bytes()).collect();
    let t = router.topology().unwrap().clone();

    // right after a cluster forms a replica may not be listed as online yet
    let replicas_online = t.shards.iter().all(|s| s.replicas().any(|r| r.online));
    for target in [ScanTarget::Primary, ScanTarget::Replica] {
        if target == ScanTarget::Replica && !replicas_online {
            continue;
        }
        let plan = targets(&manager, &caps, Some(&t), target, ScanProfile::Fast).await.unwrap();
        assert_eq!(plan.nodes.len(), 3, "{:?}", plan.notes);
        let want = if target == ScanTarget::Primary {
            Role::Primary
        } else {
            Role::Replica
        };
        assert!(plan.nodes.iter().all(|n| n.role == want));
        let mut pager = Pager::new(plan.nodes, None, None);
        let opts = PageOptions::default();
        let mut all: Vec<Vec<u8>> = Vec::new();
        let mut visited = HashSet::new();
        while !pager.is_done() {
            let (label, at, of) = pager.position();
            assert_eq!(of, 3);
            assert!(at < 3);
            visited.insert(label.unwrap().to_string());
            let p = pager.next_page(&opts).await.unwrap();
            assert!(p.keys.len() <= 200);
            all.extend(p.keys);
        }
        assert_eq!(visited.len(), 3, "{visited:?}");
        assert_eq!(all.len(), 3000, "{target:?}");
        assert_eq!(all.into_iter().collect::<HashSet<_>>(), expected);
    }

    let plan = targets(&manager, &caps, Some(&t), ScanTarget::Auto, ScanProfile::Fast)
        .await
        .unwrap();
    let mut pager = Pager::new(plan.nodes, Some(b"order:*".to_vec()), None);
    let mut orders = 0;
    while !pager.is_done() {
        orders += pager.next_page(&PageOptions::default()).await.unwrap().keys.len();
    }
    assert_eq!(orders, 1000);
    router.flush(false).await.unwrap();
}

/// A name that is not UTF-8 hashes and routes by its bytes, hashtag
/// included; its lossy rendering is another key in another slot.
#[tokio::test]
async fn names_that_are_not_utf8_route_to_their_slot() {
    use keyfold_core::meta;
    use keyfold_core::mutate::{self, Op};
    let Some((url, _guard)) = cluster_url().await else { return };
    let (_manager, mut router, _caps) = connect(&url).await;
    router.flush(false).await.unwrap();
    let names: Vec<Vec<u8>> = (0u8..24)
        .map(|i| [&b"bin:\xff"[..], &[0x80 | i, i], b":\xfe"].concat())
        .chain([b"{\xff\x00}:a".to_vec(), b"{\xff\x00}:b\x80".to_vec()])
        .collect();
    let mut owners = std::collections::BTreeSet::new();
    for k in &names {
        let cmd = testing::cluster_keyslot(k);
        let seed = router.seed().clone();
        let v = routed!(router, Hop::node(&seed), |c| c.raw(&cmd).await).unwrap();
        assert_eq!(Value::Int(key_slot(k) as i64), v, "slot of {k:?}");
        let op = Op::SetString {
            value: b"v".to_vec(),
            keep_ttl: false,
        };
        let o = routed!(router, Hop::key(k), |c| mutate::apply(c, k, &op, true).await).unwrap();
        assert_eq!(o.added, vec![k.clone()]);
        owners.insert(router.addr_for_key(k).unwrap());
    }
    assert!(owners.len() >= 2, "the names should spread over the shards: {owners:?}");
    assert_eq!(key_slot(b"{\xff\x00}:a"), key_slot(b"{\xff\x00}:b\x80"));
    assert_eq!(router.dbsize().await, Some(names.len() as u64));

    // one pipeline per owning node, results in the caller's order
    let lossy = String::from_utf8_lossy(&names[0]).into_owned();
    let refs: Vec<&[u8]> = names.iter().map(|k| &k[..]).chain([lossy.as_bytes()]).collect();
    let metas = meta::fetch_routed(&mut router, &refs).await.unwrap();
    assert!(
        metas[..names.len()]
            .iter()
            .all(|m| m.exists && m.key_type.as_deref() == Some("string"))
    );
    assert!(!metas[names.len()].exists);

    // a rename within the hashtag's slot, then a delete
    let (from, to): (&[u8], &[u8]) = (b"{\xff\x00}:a", b"{\xff\x00}:c\xc3");
    let op = Op::Rename { to: to.to_vec() };
    let o = routed!(router, Hop::key(from), |c| mutate::apply(c, from, &op, true).await).unwrap();
    assert_eq!((o.removed, o.added), (vec![from.to_vec()], vec![to.to_vec()]));
    let o = routed!(router, Hop::key(to), |c| mutate::apply(c, to, &Op::Delete, true).await).unwrap();
    assert_eq!(o.removed, vec![to.to_vec()]);
    assert_eq!(router.dbsize().await, Some(names.len() as u64 - 1));
    router.flush(false).await.unwrap();
}

#[tokio::test]
async fn cluster_live_updates_come_from_every_primary() {
    use keyfold_core::notify::{self, LiveConfig, LivePhase};
    let Some((url, _guard)) = cluster_url().await else { return };
    let (manager, mut router, _caps) = connect(&url).await;
    router.flush(false).await.unwrap();
    let primaries = router.primaries();
    assert_eq!(primaries.len(), 3);
    let mut originals = Vec::new();
    for addr in &primaries {
        let setting = routed!(router, Hop::node(addr), |c| notify::setting(c).await).unwrap();
        originals.push((addr.clone(), setting.value.clone().unwrap()));
        if setting.keyevents_enabled() != Some(true) {
            routed!(router, Hop::node(addr), |c| notify::enable(c).await).unwrap();
        }
    }
    let (handle, mut events) = notify::start(
        manager.clone(),
        primaries.clone(),
        0,
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
    .expect("all three subscriptions live");
    assert_eq!(status.borrow().nodes, 3);

    // one key per shard, so each primary publishes
    let keys: Vec<String> = sample_keys(60);
    let mut touched: std::collections::BTreeSet<_> = Default::default();
    for k in &keys {
        touched.insert(router.addr_for_key(k.as_bytes()).unwrap());
        let cmd = testing::set(k.as_bytes(), b"1");
        routed!(router, Hop::key(k.as_bytes()), |c| c.raw(&cmd).await).unwrap();
    }
    assert_eq!(touched.len(), 3);
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while seen.len() < 60 {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(batch)) => seen.extend(batch),
            _ => break,
        }
    }
    assert_eq!(seen.len(), 60, "{seen:?}");
    assert!(seen.iter().all(|e| e.event == "set" && !e.gone));
    let mut got: Vec<String> = seen.iter().map(|e| String::from_utf8(e.key.clone()).unwrap()).collect();
    got.sort();
    let mut want = keys.clone();
    want.sort();
    assert_eq!(got, want);

    handle.stop();
    for (addr, value) in originals {
        routed!(router, Hop::node(&addr), |c| c
            .raw(&commands::config_set(notify::PARAMETER, &value))
            .await)
        .unwrap();
    }
    router.flush(false).await.unwrap();
}

/// The database selector on a cluster: `INFO keyspace` summed over the
/// primaries, and at least database 0.
#[tokio::test]
async fn cluster_databases_sum_over_the_primaries() {
    let Some((url, _guard)) = cluster_url().await else { return };
    let (_manager, mut router, _caps) = connect(&url).await;
    fill(&mut router, &sample_keys(900)).await;
    let dbs = router.databases().await;
    assert!(dbs.count >= 1, "{dbs:?}");
    assert_eq!(dbs.keys.get(&0), Some(&900), "{dbs:?}");
}
