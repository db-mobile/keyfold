//! CLI harness for the backend: connect, scan into an index, inspect the
//! tree, generate test data, and report benchmark numbers.

mod generator;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use keyfold_core::index::{Index, IndexOptions, Location, Sort};
use keyfold_core::scan::{self, ScanConfig, ScanPhase, ScanProfile, ScanProgress};
use keyfold_core::{ConnectionManager, Profile, Router, ScanTarget, Tag, capability, sysinfo};

#[derive(Parser)]
#[command(name = "keyfold", version, about = "Backend harness for the key browser")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Target {
    /// Server URL, e.g. redis://localhost:6379/0 or valkey://host/1
    url: String,
    /// Environment tag: dev, staging, prod (prod defaults to polite + ephemeral)
    #[arg(long, default_value = "dev")]
    tag: String,
    /// Reach the server through an SSH bastion: user@host[:port]. The
    /// password or key passphrase is read from KEYFOLD_SSH_PASSWORD; with
    /// neither that nor --ssh-key the SSH agent is asked
    #[arg(long)]
    ssh: Option<String>,
    /// Private key file for --ssh
    #[arg(long, requires = "ssh")]
    ssh_key: Option<PathBuf>,
    /// Accept the bastion's host key when it is not known yet, and record
    /// it in the known-hosts file of this tool
    #[arg(long, requires = "ssh")]
    ssh_trust: bool,
}

impl Target {
    /// A target that is only a URL and a tag, with no bastion.
    fn plain(url: String, tag: String) -> Target {
        Target {
            url,
            tag,
            ssh: None,
            ssh_key: None,
            ssh_trust: false,
        }
    }

    fn profile(&self) -> Result<Profile> {
        let mut p = Profile::from_url(&self.url)?;
        p.tag = Tag::parse(&self.tag).with_context(|| format!("unknown tag {:?}", self.tag))?;
        if let Some(bastion) = &self.ssh {
            use keyfold_core::tunnel::{SshAuth, SshConfig, Tunnel, trust_host_key};
            let secret = std::env::var("KEYFOLD_SSH_PASSWORD").ok().filter(|s| !s.is_empty());
            let auth = match (&self.ssh_key, secret) {
                (Some(path), passphrase) => SshAuth::KeyFile {
                    path: path.clone(),
                    passphrase,
                },
                (None, Some(password)) => SshAuth::Password(password),
                (None, None) => SshAuth::Agent,
            };
            let mut config = SshConfig::parse(bastion, auth)?;
            let known_hosts = std::env::temp_dir().join("keyfold-cli-known_hosts");
            config.known_hosts = Some(known_hosts.clone());
            // log in once now: an unknown host key is a question for the user, not a failed connect
            let probe = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(Tunnel::open(config.clone())));
            match probe {
                Ok(_) => {}
                Err(keyfold_core::Error::HostKey(key)) if self.ssh_trust && !key.changed => {
                    eprintln!("trusting the host key of {}:{}: {}", key.host, key.port, key.fingerprint);
                    trust_host_key(&known_hosts, &key)?;
                }
                Err(keyfold_core::Error::HostKey(key)) if !key.changed => {
                    bail!("{} Pass --ssh-trust to accept it.", keyfold_core::Error::HostKey(key))
                }
                Err(e) => return Err(e.into()),
            }
            p.ssh = Some(config);
        }
        Ok(p)
    }
}

#[derive(Args, Clone)]
struct IndexArgs {
    /// Index file. Defaults to .local/index/<host>_<port>_<db>.sqlite
    #[arg(long)]
    index: Option<PathBuf>,
    /// Keep the index in memory and discard it at exit
    #[arg(long)]
    ephemeral: bool,
    /// Total SQLite page cache in MB across the index files
    #[arg(long, default_value_t = 128)]
    cache_mb: u32,
    /// Node shard files for a new index (existing indexes keep theirs)
    #[arg(long, default_value_t = 4)]
    shards: usize,
}

impl IndexArgs {
    fn open(&self, profile: &Profile) -> Result<Index> {
        let location = if self.ephemeral || (self.index.is_none() && profile.ephemeral_index()) {
            Location::Memory
        } else {
            Location::File(
                self.index
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".local/index").join(format!("{}.sqlite", profile.id))),
            )
        };
        Ok(Index::open(
            location,
            &IndexOptions {
                cache_mb: self.cache_mb,
                shards: self.shards,
            },
        )?)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Connect and print what the capability layer learned
    Info(Target),
    /// Run a full (or MATCH-restricted) scan into the index and print node counts
    Scan {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        index: IndexArgs,
        /// Scan profile: fast, normal, polite
        #[arg(long)]
        profile: Option<String>,
        /// Nodes to scan: auto (replica when reachable), primary, replica
        #[arg(long, default_value = "auto")]
        scan_target: String,
        /// Gap between SCAN calls in milliseconds, overrides the profile
        #[arg(long)]
        gap_ms: Option<u64>,
        /// Start a new generation instead of resuming an interrupted scan
        #[arg(long)]
        restart: bool,
        /// Drop the index before scanning
        #[arg(long)]
        clear: bool,
        /// SCAN MATCH pattern (subtree rescan; never prunes)
        #[arg(long = "match")]
        pattern: Option<String>,
        /// SCAN TYPE filter (never prunes)
        #[arg(long = "type")]
        type_filter: Option<String>,
        /// Scan the subtree under this literal prefix (SCAN MATCH prefix*):
        /// exact under the prefix afterwards, the rest of the index untouched
        #[arg(long, conflicts_with_all = ["pattern", "type_filter", "sample"])]
        subtree: Option<String>,
        /// Stop after about this many keys: seeds the tree, establishes nothing
        #[arg(long, conflicts_with_all = ["pattern", "type_filter"])]
        sample: Option<u64>,
        /// Force the namespace delimiter instead of detecting it
        #[arg(long)]
        delimiter: Option<char>,
        /// Stop after this many SCAN calls (for resume testing)
        #[arg(long)]
        stop_after_calls: Option<u64>,
        /// Top-level nodes to print at the end
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Keys buffered per merge into the index (memory versus rewrites)
        #[arg(long, default_value_t = 2_000_000)]
        merge_keys: usize,
    },
    /// Run one command as the console would: classify, route, send, render
    Exec {
        #[command(flatten)]
        target: Target,
        /// Send a command without keys to this node (host:port) of a cluster
        #[arg(long)]
        node: Option<String>,
        /// Answer the confirmation a write or a dangerous command asks for
        #[arg(long)]
        yes: bool,
        /// The connection's name, for the commands that want it typed (FLUSHDB, ...)
        #[arg(long)]
        confirm: Option<String>,
        /// The command: its arguments, or one quoted line as typed in the console
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Page through key names with plain SCAN, without any index
    Keys {
        #[command(flatten)]
        target: Target,
        /// Scan profile: fast, normal, polite
        #[arg(long)]
        profile: Option<String>,
        /// Nodes to read: auto (replica when reachable), primary, replica
        #[arg(long, default_value = "auto")]
        scan_target: String,
        /// SCAN MATCH glob; the server still walks every key
        #[arg(long = "match")]
        pattern: Option<String>,
        /// SCAN TYPE filter
        #[arg(long = "type")]
        type_filter: Option<String>,
        /// Keys per page
        #[arg(long, default_value_t = 200)]
        page: usize,
        /// Pages to read; 0 reads to the end
        #[arg(long, default_value_t = 1)]
        pages: u64,
        /// Print only the totals, not the names
        #[arg(long)]
        quiet: bool,
    },
    /// Print children of a tree node from an existing index, no server needed
    Tree {
        /// Index file
        index: PathBuf,
        /// Node prefix including the trailing delimiter, e.g. "user:" ("" = root)
        #[arg(default_value = "")]
        prefix: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Continue after this child segment (keyset paging)
        #[arg(long)]
        after: Option<String>,
        /// Sort: name, count, numeric
        #[arg(long, default_value = "count")]
        sort: String,
    },
    /// Fold decision and fold queries for one level of an existing index, timed
    Fold {
        index: PathBuf,
        /// Node prefix including the trailing delimiter, e.g. "user:"
        prefix: String,
    },
    /// Rebuild the tree of an existing index with another delimiter
    Reindex { index: PathBuf, delimiter: char },
    /// Search an existing index (glob, substring or /regex/), timed; no server needed
    Search {
        index: PathBuf,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Also count every match
        #[arg(long)]
        count: bool,
        /// Verify the result against the server with SCAN MATCH and reconcile
        #[arg(long)]
        verify: Option<String>,
    },
    /// Delete every key an index query matches: preview (count and sample), then batched UNLINK with --yes
    Delete {
        index: PathBuf,
        query: String,
        /// Server URL to delete on
        #[arg(long)]
        url: String,
        #[arg(long, default_value = "dev")]
        tag: String,
        /// Actually delete; without it only the preview is printed
        #[arg(long)]
        yes: bool,
        /// Keys per UNLINK call
        #[arg(long, default_value_t = 500)]
        batch: usize,
    },
    /// Subscribe to keyspace notifications and print (or apply) the events
    Watch {
        #[command(flatten)]
        target: Target,
        /// Apply the events to this index instead of printing them
        #[arg(long)]
        index: Option<PathBuf>,
        /// Turn notify-keyspace-events on at the server first (CONFIG SET)
        #[arg(long)]
        enable: bool,
        /// Stop after this many seconds
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Build or drop the trigram table for instant substring search (CONCEPT 3.3)
    Fts {
        index: PathBuf,
        /// Drop the table instead of building it
        #[arg(long)]
        drop: bool,
    },
    /// Offline ingestion benchmark: generated keys in hash-like order, no server
    BenchIngest {
        /// Index file (deleted first)
        #[arg(long, default_value = ".local/bench/ingest.sqlite")]
        index: PathBuf,
        #[arg(long, default_value_t = 2_000_000)]
        keys: u64,
        /// Keys per merge
        #[arg(long, default_value_t = 1_000_000)]
        merge: usize,
        #[arg(long, default_value_t = 128)]
        cache_mb: u32,
        #[arg(long, default_value_t = 4)]
        shards: usize,
        /// Use the parallel writer threads (as the scan engine does)
        #[arg(long)]
        parallel: bool,
    },
    /// Fill a database with generated keys (test data only)
    Gen {
        #[command(flatten)]
        target: Target,
        /// Number of keys to write
        #[arg(long, default_value_t = 1_000_000)]
        keys: u64,
        /// SET commands per pipeline
        #[arg(long, default_value_t = 10_000)]
        batch: usize,
        /// FLUSHDB first
        #[arg(long)]
        flush: bool,
        /// Also write this many keys whose names are not valid UTF-8
        #[arg(long, default_value_t = 0)]
        binary: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Info(target) => info(target).await,
        Command::Scan {
            target,
            index,
            profile,
            scan_target,
            gap_ms,
            restart,
            clear,
            pattern,
            type_filter,
            subtree,
            sample,
            delimiter,
            stop_after_calls,
            top,
            merge_keys,
        } => {
            run_scan(
                target,
                index,
                profile,
                scan_target,
                gap_ms,
                restart,
                clear,
                pattern,
                type_filter,
                subtree,
                sample,
                delimiter,
                stop_after_calls,
                top,
                merge_keys,
            )
            .await
        }
        Command::Exec {
            target,
            node,
            yes,
            confirm,
            command,
        } => exec(target, node, yes, confirm, command).await,
        Command::Keys {
            target,
            profile,
            scan_target,
            pattern,
            type_filter,
            page,
            pages,
            quiet,
        } => run_keys(target, profile, scan_target, pattern, type_filter, page, pages, quiet).await,
        Command::Tree {
            index,
            prefix,
            limit,
            after,
            sort,
        } => tree(index, prefix, limit, after, sort),
        Command::Fold { index, prefix } => fold_level(index, prefix),
        Command::Search {
            index,
            query,
            limit,
            count,
            verify,
        } => search(index, query, limit, count, verify).await,
        Command::Delete {
            index,
            query,
            url,
            tag,
            yes,
            batch,
        } => delete(index, query, Target::plain(url, tag), yes, batch).await,
        Command::Watch {
            target,
            index,
            enable,
            seconds,
        } => watch(target, index, enable, seconds).await,
        Command::Fts { index, drop } => fts(index, drop),
        Command::Reindex { index, delimiter } => {
            let mut idx = Index::open(Location::File(index), &IndexOptions::default())?;
            let t = std::time::Instant::now();
            idx.rebuild_nodes(delimiter as u8)?;
            println!("rebuilt nodes with delimiter {delimiter:?} in {:.1?}", t.elapsed());
            Ok(())
        }
        Command::Gen {
            target,
            keys,
            batch,
            flush,
            binary,
        } => generate(target, keys, batch, flush, binary).await,
        Command::BenchIngest {
            index,
            keys,
            merge,
            cache_mb,
            shards,
            parallel,
        } => bench_ingest(index, keys, merge, cache_mb, shards, parallel),
    }
}

async fn info(target: Target) -> Result<()> {
    let profile = target.profile()?;
    let manager = ConnectionManager::new(profile.clone());
    let t = std::time::Instant::now();
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    println!("connected to {} in {:.1?}", conn.label(), t.elapsed());
    println!("server    {} {}", caps.server, caps.version);
    println!("mode      {}   role {}   protocol RESP{}", caps.mode, caps.role, caps.protocol);
    println!(
        "db        {}   dbsize {}",
        caps.db,
        caps.dbsize.map_or("denied".to_string(), fmt_count)
    );
    println!("memory    {}", caps.used_memory.map_or("unknown".to_string(), fmt_bytes));
    for r in &caps.replicas {
        println!("replica   {}   {}", r.addr, r.state);
    }
    if let Some(p) = &caps.primary {
        println!("primary   {p}");
    }
    let mut router = Router::new(manager.clone(), conn, &caps).await;
    if let Some(note) = router.note() {
        println!("note      {note}");
    }
    if let Some(t) = router.topology() {
        println!(
            "cluster   {} shards, {} nodes, {} unassigned slots",
            t.shards.len(),
            t.node_count(),
            t.unassigned_slots()
        );
        for (i, shard) in t.shards.iter().enumerate() {
            let slots: Vec<String> = shard.slots.iter().map(|(lo, hi)| format!("{lo}-{hi}")).collect();
            println!("  shard {}  slots {}", i + 1, slots.join(","));
            for n in &shard.nodes {
                println!(
                    "    {:<8} {:<22} {} {}",
                    n.role.as_str(),
                    n.addr.to_string(),
                    n.id,
                    if n.online { "online" } else { "offline" }
                );
            }
        }
        println!(
            "dbsize    {} (sum over primaries)",
            router.dbsize().await.map_or("denied".to_string(), fmt_count)
        );
    }
    let setting = {
        let seed = router.seed().clone();
        let conn = router.node(&seed).await?;
        keyfold_core::notify::setting(conn).await?
    };
    println!(
        "notify    {}",
        match setting.keyevents_enabled() {
            Some(true) => format!("on ({})", setting.value.unwrap_or_default()),
            Some(false) => format!(
                "off ({:?}); `keyfold watch --enable` turns key events on",
                setting.value.unwrap_or_default()
            ),
            None => "unknown (CONFIG GET denied)".into(),
        }
    );
    println!("commands");
    for (name, state) in &caps.commands {
        println!("  {name:<18} {state:?}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_scan(
    target: Target,
    index_args: IndexArgs,
    profile_name: Option<String>,
    scan_target: String,
    gap_ms: Option<u64>,
    restart: bool,
    clear: bool,
    pattern: Option<String>,
    type_filter: Option<String>,
    subtree: Option<String>,
    sample: Option<u64>,
    delimiter: Option<char>,
    stop_after_calls: Option<u64>,
    top: usize,
    merge_keys: usize,
) -> Result<()> {
    let profile = target.profile()?;
    let scan_profile = match profile_name {
        Some(p) => ScanProfile::parse(&p).with_context(|| format!("unknown scan profile {p:?}"))?,
        None => profile.scan_profile(),
    };
    let scan_target = ScanTarget::parse(&scan_target).with_context(|| format!("unknown scan target {scan_target:?}"))?;
    let mut index = index_args.open(&profile)?;
    if clear {
        index.clear()?;
    }
    eprintln!(
        "index: {:?}  (existing keys: {})",
        index.location(),
        fmt_count(index.key_count() as u64)
    );

    let manager = ConnectionManager::new(profile.clone());
    let t0 = std::time::Instant::now();
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    eprintln!(
        "connected: {} {} ({}, {}) in {:.1?}",
        caps.server,
        caps.version,
        caps.mode,
        caps.role,
        t0.elapsed()
    );
    let router = Router::new(manager.clone(), conn, &caps).await;
    if let Some(note) = router.note() {
        eprintln!("note: {note}");
    }
    let plan = scan::targets(&manager, &caps, router.topology(), scan_target, scan_profile).await?;
    for n in &plan.notes {
        eprintln!("note: {n}");
    }
    eprintln!(
        "scanning {} with the {} profile",
        scan::describe(&plan.nodes),
        scan_profile.as_str()
    );

    let config = ScanConfig {
        profile: scan_profile,
        gap: gap_ms.map(Duration::from_millis),
        restart,
        pattern: pattern.map(String::into_bytes),
        type_filter,
        subtree: subtree.map(String::into_bytes),
        sample,
        delimiter: delimiter.map(|c| c as u8),
        merge_max_keys: merge_keys.max(1),
        ..ScanConfig::default()
    };
    let handle = scan::start_nodes(plan.nodes, index, config);
    let mut progress = handle.progress();
    let mut last_line = String::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut warned_resume = false;
    loop {
        tokio::select! {
            changed = progress.changed() => {
                if changed.is_err() { break; }
                let p = progress.borrow().clone();
                if p.resumed && !warned_resume {
                    warned_resume = true;
                    eprintln!("resuming generation {} from a persisted cursor; keys created since the interruption may be missed", p.generation);
                }
                if let Some(n) = stop_after_calls
                    && p.calls >= n && matches!(p.phase, ScanPhase::Scanning) {
                        handle.stop();
                    }
                if matches!(p.phase, ScanPhase::Complete | ScanPhase::Stopped | ScanPhase::Failed) { break; }
            }
            _ = ticker.tick() => {
                let line = progress_line(&progress.borrow());
                if line != last_line {
                    eprint!("\r{line:<110}");
                    last_line = line;
                }
            }
        }
    }
    eprintln!();
    let outcome = handle.wait().await?;
    let p = &outcome.progress;
    let index = outcome.index;
    index.checkpoint()?;

    println!(
        "phase        {:?}{}",
        p.phase,
        match (&p.subtree, p.sample) {
            (Some(prefix), _) => format!(
                " (subtree {:?}: exact under it, the rest untouched)",
                String::from_utf8_lossy(prefix)
            ),
            (None, Some(n)) => format!(" (sample of about {}: nothing established)", fmt_count(n)),
            _ if p.partial => " (partial: no pruning)".to_string(),
            _ => String::new(),
        }
    );
    println!("generation   {}", p.generation);
    println!("elapsed      {:.1?}", p.elapsed);
    if p.nodes.len() > 1 || p.backoff > 0 {
        for n in &p.nodes {
            println!(
                "node         {:<22} {:<8} scanned {:>12}   calls {:>6}   count {:>5}{}",
                n.label,
                n.role.as_str(),
                fmt_count(n.scanned),
                n.calls,
                n.count,
                n.error.as_ref().map(|e| format!("   error {e}")).unwrap_or_default()
            );
        }
    }
    println!(
        "scan calls   {}   final COUNT {}   baseline rtt {:.0?}   last rtt {:.1?}",
        p.calls, p.count, p.baseline_rtt, p.last_rtt
    );
    println!(
        "scanned      {}   ({} new to the index, {} pruned)",
        fmt_count(p.scanned),
        fmt_count(p.indexed_new),
        fmt_count(p.pruned)
    );
    if let Some(d) = p.dbsize {
        println!("dbsize       {}", fmt_count(d));
    }
    if p.elapsed.as_secs_f64() > 0.0 {
        println!(
            "throughput   {} keys/s",
            fmt_count((p.scanned as f64 / p.elapsed.as_secs_f64()) as u64)
        );
    }
    println!(
        "index keys   {}   delimiter {:?}",
        fmt_count(index.key_count() as u64),
        index.delimiter().map(|d| d as char)
    );
    println!("index disk   {}", fmt_bytes(index.disk_size()));
    if let Some(rss) = sysinfo::rss_bytes() {
        println!("process rss  {}", fmt_bytes(rss));
    }
    let root = index.root()?;
    println!(
        "root         {} children   classes {}",
        fmt_count(root.children as u64),
        classes(&root.classes)
    );
    let t = std::time::Instant::now();
    let children = index.children(b"", None, top, Sort::Count)?;
    let took = t.elapsed();
    print_nodes(&children, index.delimiter().unwrap_or(b':'));
    println!("(root window of {} rows in {:.1?})", children.len(), took);
    Ok(())
}

fn progress_line(p: &ScanProgress) -> String {
    let pct = p
        .fraction()
        .map(|f| format!("{:5.1}%", f * 100.0))
        .unwrap_or_else(|| "  n/a ".into());
    let rate = if p.elapsed.as_secs_f64() > 0.0 {
        p.scanned as f64 / p.elapsed.as_secs_f64()
    } else {
        0.0
    };
    format!(
        "{:?} {} {} / {} | new {} | COUNT {} | {} calls | rtt {:.1?} | q{} | {:.1}s | {} k/s",
        p.phase,
        pct,
        fmt_count(p.scanned),
        p.dbsize.map_or("?".into(), fmt_count),
        fmt_count(p.indexed_new),
        p.count,
        p.calls,
        p.last_rtt,
        p.queued,
        p.elapsed.as_secs_f64(),
        (rate / 1000.0) as u64,
    )
}

fn tree(path: PathBuf, prefix: String, limit: usize, after: Option<String>, sort: String) -> Result<()> {
    let sort = match sort.as_str() {
        "name" => Sort::Name,
        "count" => Sort::Count,
        "numeric" => Sort::Numeric,
        other => bail!("unknown sort {other:?}"),
    };
    let index = Index::open(Location::File(path), &IndexOptions::default())?;
    let delimiter = index.delimiter().unwrap_or(b':');
    let state = index.scan_state()?;
    println!(
        "index: {} keys, delimiter {:?}, generation {} {:?}{}",
        fmt_count(index.key_count() as u64),
        delimiter as char,
        state.generation,
        state.status,
        state.finished_at.map(|t| format!(", scanned at {t}")).unwrap_or_default()
    );
    println!("coverage: {}", coverage_line(&index, prefix.as_bytes())?);
    if prefix.is_empty() {
        let root = index.root()?;
        println!(
            "root: {} children, classes {}",
            fmt_count(root.children as u64),
            classes(&root.classes)
        );
    } else {
        let body = prefix.strip_suffix(delimiter as char).unwrap_or(&prefix);
        let (parent, segment) = match body.rfind(delimiter as char) {
            Some(i) => (&body[..=i], &body[i + 1..]),
            None => ("", body),
        };
        match index.node(parent.as_bytes(), segment.as_bytes())? {
            Some(n) => println!(
                "node {:?}: {} keys, {} children, classes {}",
                prefix,
                fmt_count(n.count as u64),
                fmt_count(n.children as u64),
                classes(&n.classes)
            ),
            None => println!("node {prefix:?}: not in index"),
        }
    }
    let after_node = match after {
        Some(seg) => index.node(prefix.as_bytes(), seg.as_bytes())?,
        None => None,
    };
    let t = std::time::Instant::now();
    let nodes = index.children(prefix.as_bytes(), after_node.as_ref(), limit, sort)?;
    let took = t.elapsed();
    print_nodes(&nodes, delimiter);
    println!(
        "({} rows in {:.1?}; continue with --after {:?})",
        nodes.len(),
        took,
        nodes
            .last()
            .map(|n| String::from_utf8_lossy(&n.segment).into_owned())
            .unwrap_or_default()
    );
    Ok(())
}

fn fold_level(path: PathBuf, prefix: String) -> Result<()> {
    use keyfold_core::fold::{self, FoldThresholds};
    use keyfold_core::index::ChildFilter;
    use keyfold_core::segment::SegClass;
    let index = Index::open(Location::File(path), &IndexOptions::default())?;
    let delimiter = index.delimiter().unwrap_or(b':');
    let p = prefix.as_bytes();
    let (distinct, classes) = if p.is_empty() {
        let r = index.root()?;
        (r.children, r.classes)
    } else {
        let body = &p[..p.len() - 1];
        let (parent, segment) = match body.iter().rposition(|&b| b == delimiter) {
            Some(i) => (&body[..=i], &body[i + 1..]),
            None => (&body[..0], body),
        };
        let n = index.node(parent, segment)?.context("node not in index")?;
        (n.children, n.classes)
    };
    let t = std::time::Instant::now();
    let thresholds = FoldThresholds::default();
    let similarity = if fold::needs_similarity(distinct, &classes, &thresholds) {
        index.similarity(p, delimiter, 200)?
    } else {
        None
    };
    let d = fold::decide(distinct, &classes, similarity, &thresholds, None);
    println!("decision ({:.1?}): folded={} {}", t.elapsed(), d.is_folded(), d.evidence);
    for f in &d.folds {
        let class = f.class.seg_class();
        let t = std::time::Instant::now();
        let range = index.numeric_range(p)?;
        println!(
            "  fold {} members {} share {:.3} range {:?} ({:.1?})",
            f.class.label(None, range),
            fmt_count(f.members as u64),
            f.share,
            range,
            t.elapsed()
        );
        let t = std::time::Instant::now();
        let s = index.fold_summary(p, delimiter, class, 10)?;
        println!(
            "  summary: {} members with children, {} distinct segments ({:.1?})",
            fmt_count(s.members_with_children as u64),
            s.distinct_segments,
            t.elapsed()
        );
        for e in &s.entries {
            println!(
                "    {:<30} {:>10} parents {:>10} keys",
                String::from_utf8_lossy(&e.segment),
                fmt_count(e.parents as u64),
                fmt_count(e.keys as u64)
            );
        }
        let t = std::time::Instant::now();
        if let Some((lo, hi)) = range {
            let b = index.numeric_buckets(p, lo, hi)?;
            println!("  {} numeric buckets ({:.1?})", b.len(), t.elapsed());
        } else {
            let b = index.leading_buckets(p)?;
            println!("  {} leading-char buckets ({:.1?})", b.len(), t.elapsed());
        }
        let t = std::time::Instant::now();
        let samples = index.sample_children(p, class, 3)?;
        println!(
            "  samples {:?} ({:.1?})",
            samples.iter().map(|s| String::from_utf8_lossy(s).into_owned()).collect::<Vec<_>>(),
            t.elapsed()
        );
        let t = std::time::Instant::now();
        let exclude: Vec<SegClass> = class.into_iter().collect();
        let literals = index.children_filtered(
            p,
            None,
            200,
            Sort::Name,
            &ChildFilter {
                exclude_classes: exclude,
                ..Default::default()
            },
        )?;
        println!("  {} literal siblings ({:.1?})", literals.len(), t.elapsed());
    }
    Ok(())
}

async fn search(path: PathBuf, query: String, limit: usize, count: bool, verify: Option<String>) -> Result<()> {
    use keyfold_core::search::{self, Plan, VerifyOptions};
    use std::sync::atomic::AtomicBool;
    let mut index = Index::open(Location::File(path), &IndexOptions::default())?;
    let plan = Plan::new(search::parse(&query))?;
    println!(
        "mode {}  range {:?}..{:?}  like {:?}  fts {}",
        plan.query.mode.name(),
        String::from_utf8_lossy(&plan.lo),
        plan.hi.as_deref().map(String::from_utf8_lossy),
        plan.like.as_deref().map(String::from_utf8_lossy),
        if index.has_fts() && plan.fts_literal.is_some() {
            "possible"
        } else {
            "no"
        }
    );
    let cancel = AtomicBool::new(false);
    let t = std::time::Instant::now();
    let page = index.search(&plan, None, limit, &cancel)?;
    let took = t.elapsed();
    for r in &page.rows {
        println!("  {}", String::from_utf8_lossy(&r.name));
    }
    println!(
        "{} rows in {:.1?} (examined {}, done {}, via fts {})",
        page.rows.len(),
        took,
        page.examined,
        page.done,
        page.via_fts
    );
    if count {
        let t = std::time::Instant::now();
        let n = index.search_count(&plan, &cancel)?;
        println!("count {:?} in {:.1?}", n, t.elapsed());
    }
    if let Some(url) = verify {
        let pattern = plan
            .server_pattern()
            .context("regular expressions cannot be verified on the server")?;
        let target = Target::plain(url, "dev".into());
        let profile = target.profile()?;
        let manager = ConnectionManager::new(profile.clone());
        let mut conn = manager.connect().await?;
        let caps = capability::probe(&mut conn, profile.db).await?;
        let mut router = Router::new(manager, conn, &caps).await;
        let opts = VerifyOptions {
            gap: if profile.tag == Tag::Prod {
                ScanProfile::Polite.gap()
            } else {
                Duration::ZERO
            },
            ..VerifyOptions::default()
        };
        let t = std::time::Instant::now();
        let report = search::verify(&mut router, &mut index, &pattern, &opts, &cancel).await?;
        println!("verify {:?} in {:.1?}", report, t.elapsed());
    }
    Ok(())
}

async fn delete(path: PathBuf, query: String, target: Target, yes: bool, batch: usize) -> Result<()> {
    use keyfold_core::mutate::{self, DeleteConfig, DeletePhase};
    use keyfold_core::search::{self, Plan};
    use std::sync::atomic::AtomicBool;
    let index = Index::open(Location::File(path), &IndexOptions::default())?;
    let plan = Plan::new(search::parse(&query))?;
    let never = AtomicBool::new(false);
    let t = std::time::Instant::now();
    let count = index.search_count(&plan, &never)?.unwrap_or(0);
    let sample = index.search(&plan, None, 20, &never)?;
    println!(
        "{} mode: {} keys match in the index ({:.1?})",
        plan.query.mode.name(),
        fmt_count(count as u64),
        t.elapsed()
    );
    for r in &sample.rows {
        println!("  {}", String::from_utf8_lossy(&r.name));
    }
    if count > sample.rows.len() as i64 {
        println!("  … and {} more", fmt_count((count - sample.rows.len() as i64) as u64));
    }
    if !yes {
        println!("nothing deleted; run again with --yes to delete these keys with batched UNLINK");
        return Ok(());
    }
    let profile = target.profile()?;
    let manager = ConnectionManager::new(profile.clone());
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    let config = DeleteConfig {
        batch,
        gap: if profile.tag == Tag::Prod {
            ScanProfile::Polite.gap()
        } else {
            Duration::ZERO
        },
        use_unlink: caps.has("unlink"),
    };
    let router = Router::new(manager, conn, &caps).await;
    let handle = mutate::start_delete(router, index, plan, config);
    let mut progress = handle.progress();
    let mut last = std::time::Instant::now();
    while progress.changed().await.is_ok() {
        let p = progress.borrow().clone();
        if last.elapsed() > Duration::from_millis(250) || matches!(p.phase, DeletePhase::Complete | DeletePhase::Failed) {
            eprintln!(
                "  {:?}: {} of {} deleted, {} calls",
                p.phase,
                fmt_count(p.deleted),
                p.total.map(|n| fmt_count(n as u64)).unwrap_or_else(|| "?".into()),
                p.calls
            );
            last = std::time::Instant::now();
        }
    }
    let out = handle.wait().await?;
    let p = out.progress;
    println!(
        "{:?}: {} deleted on the server, {} removed from the index, {} calls in {:.1?}{}",
        p.phase,
        fmt_count(p.deleted),
        fmt_count(p.removed),
        p.calls,
        p.elapsed,
        p.error.map(|e| format!(" ({e})")).unwrap_or_default()
    );
    Ok(())
}

/// Keyspace notifications: subscribe on every primary, print the events
/// or apply them to an index (CONCEPT 7, "live").
async fn watch(target: Target, index: Option<PathBuf>, enable: bool, seconds: Option<u64>) -> Result<()> {
    use keyfold_core::notify::{self, LiveConfig, LivePhase};
    let profile = target.profile()?;
    let manager = ConnectionManager::new(profile.clone());
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    let mut router = Router::new(manager.clone(), conn, &caps).await;
    let primaries = router.primaries();
    for addr in &primaries {
        let c = router.node(addr).await?;
        let setting = notify::setting(c).await?;
        match (setting.keyevents_enabled(), enable) {
            (Some(true), _) => eprintln!("{addr}: notify-keyspace-events = {}", setting.value.unwrap_or_default()),
            (Some(false), true) => {
                let v = notify::enable(c).await?;
                eprintln!("{addr}: set notify-keyspace-events = {v}");
            }
            (Some(false), false) => eprintln!(
                "{addr}: notify-keyspace-events = {:?}: key events are off; nothing will arrive (use --enable)",
                setting.value.unwrap_or_default()
            ),
            (None, _) => eprintln!("{addr}: CONFIG GET denied; subscribing anyway"),
        }
    }
    let mut index = match index {
        Some(p) => Some(Index::open(Location::File(p), &IndexOptions::default())?),
        None => None,
    };
    let (handle, mut events) = notify::start(manager, primaries, profile.db, LiveConfig::default());
    let mut status = handle.status();
    let deadline = seconds.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut applied = (0u64, 0u64);
    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() { break; }
                let s = status.borrow().clone();
                eprintln!("status: {} ({} of {} nodes, {} events{})", s.phase.as_str(), s.nodes_live, s.nodes, s.events, s.error.map(|e| format!(", {e}")).unwrap_or_default());
                if s.phase == LivePhase::Stopped { break; }
            }
            batch = events.recv() => {
                let Some(batch) = batch else { break };
                match &mut index {
                    Some(idx) => {
                        let outcomes = notify::reduce(&batch);
                        for o in &outcomes {
                            let c = keyfold_core::mutate::write_through(idx, o)?;
                            applied.0 += c.added;
                            applied.1 += c.removed;
                        }
                        eprintln!("{} events: {} keys, index +{} -{}", batch.len(), outcomes.len(), applied.0, applied.1);
                    }
                    None => {
                        for e in &batch {
                            println!("{:<12} {}", e.event, String::from_utf8_lossy(&e.key));
                        }
                    }
                }
            }
            _ = sleep => {
                handle.stop();
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                handle.stop();
                break;
            }
        }
    }
    if let Some(idx) = index {
        println!("index keys {}  (+{} -{})", fmt_count(idx.key_count() as u64), applied.0, applied.1);
    }
    Ok(())
}

fn fts(path: PathBuf, drop: bool) -> Result<()> {
    let mut index = Index::open(Location::File(path), &IndexOptions::default())?;
    let t = std::time::Instant::now();
    if drop {
        index.drop_fts()?;
        println!(
            "dropped the trigram table in {:.1?}, index now {}",
            t.elapsed(),
            fmt_bytes(index.disk_size())
        );
        return Ok(());
    }
    let before = index.disk_size();
    let mut last = std::time::Instant::now();
    index.build_fts(&mut |done, total| {
        if last.elapsed() > Duration::from_secs(1) {
            eprintln!("  {} of {} keys", fmt_count(done as u64), fmt_count(total as u64));
            last = std::time::Instant::now();
        }
        true
    })?;
    index.checkpoint()?;
    println!(
        "built the trigram table in {:.1?}: index {} -> {}",
        t.elapsed(),
        fmt_bytes(before),
        fmt_bytes(index.disk_size())
    );
    Ok(())
}

/// Whether the index is exact under a prefix, and since when.
fn coverage_line(index: &Index, prefix: &[u8]) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut line = match index.coverage(prefix)? {
        Some(c) => format!(
            "exact as of {} s ago, by {}",
            (now - c.at).max(0),
            if c.by.is_empty() {
                "a full scan".to_string()
            } else {
                format!("a scan of {:?}", String::from_utf8_lossy(&c.by))
            }
        ),
        None => "not scanned: counts are lower bounds".to_string(),
    };
    let beneath: Vec<String> = index
        .subtree_scans()?
        .iter()
        .filter(|s| s.prefix.starts_with(prefix) && s.prefix != prefix)
        .map(|s| {
            format!(
                "{:?}{}",
                String::from_utf8_lossy(&s.prefix),
                if s.finished_at.is_none() { " (interrupted)" } else { "" }
            )
        })
        .collect();
    if !beneath.is_empty() {
        line.push_str(&format!("; scanned beneath: {}", beneath.join(", ")));
    }
    Ok(line)
}

fn print_nodes(nodes: &[keyfold_core::Node], delimiter: u8) {
    for n in nodes {
        let seg = String::from_utf8_lossy(&n.segment);
        let marker = if n.children > 0 { "▸" } else { " " };
        let shape = if n.children > 0 {
            format!("  {} children {}", fmt_count(n.children as u64), classes(&n.classes))
        } else {
            String::new()
        };
        let leaf = if n.leaf && n.children > 0 { " (also a key)" } else { "" };
        println!(
            "  {marker} {:<40} {:>14}{shape}{leaf}",
            format!(
                "{seg}{}",
                if n.children > 0 {
                    (delimiter as char).to_string()
                } else {
                    String::new()
                }
            ),
            fmt_count(n.count as u64)
        );
    }
}

fn classes(c: &keyfold_core::index::ClassCounts) -> String {
    let mut parts = Vec::new();
    for (name, v) in [
        ("uuid", c.uuid),
        ("ulid", c.ulid),
        ("hex", c.hex),
        ("num", c.numeric),
        ("date", c.date),
        ("opaque", c.opaque),
        ("email", c.email),
        ("literal", c.literal),
    ] {
        if v > 0 {
            parts.push(format!("{name}={}", fmt_count(v as u64)));
        }
    }
    if parts.is_empty() { "-".into() } else { parts.join(" ") }
}

/// `exec`: one command through the console's path. Exit code 2 when it was
/// refused or left unconfirmed, 1 when the server answered with an error.
async fn exec(target: Target, node: Option<String>, yes: bool, confirm: Option<String>, command: Vec<String>) -> Result<()> {
    use keyfold_core::console::{self, Action, Confirm, Console, Effect, Limits};
    let profile = target.profile()?;
    let args = match command.as_slice() {
        [line] => console::split_line(line)?,
        parts => parts.iter().map(|p| p.clone().into_bytes()).collect(),
    };
    let node = match node {
        Some(n) => {
            let (host, port) = n.rsplit_once(':').with_context(|| format!("not host:port: {n:?}"))?;
            Some(keyfold_core::NodeAddr {
                host: host.to_string(),
                port: port.parse().with_context(|| format!("not a port: {port:?}"))?,
            })
        }
        None => None,
    };
    let manager = ConnectionManager::new(profile.clone());
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    let mut console = Console::new(Router::new(manager, conn, &caps).await);
    let verdict = console.inspect(&args, profile.tag == Tag::Prod).await?;
    match &verdict.action {
        Action::Run => {}
        Action::Refuse(why) => {
            eprintln!("refused: {why}");
            std::process::exit(2);
        }
        Action::Confirm(Confirm::Typed(what)) => {
            if confirm.as_deref() != Some(profile.name.as_str()) {
                eprintln!("{what} Pass --confirm {:?} to run it.", profile.name);
                std::process::exit(2);
            }
        }
        Action::Confirm(Confirm::Danger(what)) => {
            if !yes {
                eprintln!("{what} Pass --yes to run it.");
                std::process::exit(2);
            }
        }
        Action::Confirm(Confirm::Write) => {
            if !yes {
                eprintln!("this connection is tagged prod and the command is not known to be read-only. Pass --yes to run it.");
                std::process::exit(2);
            }
        }
    }
    let ran = console.run(&args, node.as_ref(), &verdict).await?;
    let failed = ran.reply.is_err();
    match &ran.reply {
        Ok(v) => {
            for line in console::render(v, &Limits::default()).lines {
                println!("{line}");
            }
        }
        Err(e) => println!("{}", console::render_error(e)),
    }
    eprintln!("{} in {:.1?}", ran.node, ran.took);
    match &ran.effect {
        Effect::None => {}
        Effect::Flushed => eprintln!("index: the database was emptied"),
        Effect::Unknown => eprintln!("index: the command may have changed keys it did not name"),
        Effect::Keys(keys) => {
            for (k, o) in keys.iter().zip(console::reconcile(console.router(), keys).await?) {
                eprintln!(
                    "index: {} {}",
                    console::quoted(k),
                    if o.exists() {
                        match o.ttl {
                            Some(t) => format!("exists, expires in {t} s"),
                            None => "exists".to_string(),
                        }
                    } else {
                        "is gone".to_string()
                    }
                );
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// `keys`: plain SCAN pages through the nodes a scan would read.
#[allow(clippy::too_many_arguments)]
async fn run_keys(
    target: Target,
    profile_name: Option<String>,
    scan_target: String,
    pattern: Option<String>,
    type_filter: Option<String>,
    page: usize,
    pages: u64,
    quiet: bool,
) -> Result<()> {
    use keyfold_core::browse::{PageOptions, Pager};
    let profile = target.profile()?;
    let scan_profile = match profile_name {
        Some(p) => ScanProfile::parse(&p).with_context(|| format!("unknown scan profile {p:?}"))?,
        None => profile.scan_profile(),
    };
    let scan_target = ScanTarget::parse(&scan_target).with_context(|| format!("unknown scan target {scan_target:?}"))?;
    let manager = ConnectionManager::new(profile.clone());
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    let router = Router::new(manager.clone(), conn, &caps).await;
    let plan = scan::targets(&manager, &caps, router.topology(), scan_target, scan_profile).await?;
    for n in &plan.notes {
        eprintln!("note: {n}");
    }
    eprintln!("reading {} with the {} profile", scan::describe(&plan.nodes), scan_profile.as_str());
    let opts = PageOptions {
        want: page.max(1),
        gap: scan_profile.gap(),
        ..PageOptions::default()
    };
    let mut pager = Pager::new(plan.nodes, pattern.map(String::into_bytes), type_filter);
    let t = std::time::Instant::now();
    let mut read = 0u64;
    while pages == 0 || read < pages {
        let p = pager.next_page(&opts).await?;
        read += 1;
        if !quiet {
            for k in &p.keys {
                println!("{}", String::from_utf8_lossy(k));
            }
        }
        eprintln!(
            "page {read}: {} keys in {} calls{}",
            p.keys.len(),
            p.calls,
            if p.done { ", end" } else { "" }
        );
        if p.done {
            break;
        }
    }
    eprintln!(
        "{} keys in {} pages, {} SCAN calls, about {} of {} keys examined, {:.1?}{}",
        fmt_count(pager.returned()),
        read,
        pager.calls(),
        fmt_count(pager.examined()),
        caps.dbsize.map(fmt_count).unwrap_or_else(|| "?".into()),
        t.elapsed(),
        if pager.is_done() { "" } else { " (more to read)" }
    );
    Ok(())
}

async fn generate(target: Target, keys: u64, batch: usize, flush: bool, binary: u64) -> Result<()> {
    let profile = target.profile()?;
    if profile.tag == Tag::Prod {
        bail!("refusing to generate test data into a prod-tagged connection");
    }
    let manager = ConnectionManager::new(profile.clone());
    let mut conn = manager.connect().await?;
    let caps = capability::probe(&mut conn, profile.db).await?;
    let mut router = Router::new(manager, conn, &caps).await;
    let before = generator::dbsize(&mut router).await?;
    eprintln!("dbsize before: {}", fmt_count(before));
    let (sent, took) = generator::generate(&mut router, keys, batch, flush, |n| {
        if n % 1_000_000 == 0 || n == keys {
            eprint!("\r{} / {} written", fmt_count(n), fmt_count(keys));
        }
    })
    .await?;
    eprintln!();
    let sent = sent + generator::generate_binary(&mut router, binary, batch).await?;
    let after = generator::dbsize(&mut router).await?;
    println!(
        "wrote {} SETs in {:.1?} ({} keys/s); dbsize {} -> {}",
        fmt_count(sent),
        took,
        fmt_count((sent as f64 / took.as_secs_f64()) as u64),
        fmt_count(before),
        fmt_count(after)
    );
    Ok(())
}

fn bench_ingest(path: PathBuf, keys: u64, merge: usize, cache_mb: u32, shards: usize, parallel: bool) -> Result<()> {
    use rand::SeedableRng;
    use rand::seq::SliceRandom;
    let location = Location::File(path);
    for f in location.files(16) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", f.display()));
        }
    }
    let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
    let t = std::time::Instant::now();
    let mut all: Vec<Vec<u8>> = (0..keys).map(|i| generator::key_name(i, keys, &mut rng).into_bytes()).collect();
    all.shuffle(&mut rng);
    eprintln!("generated {} keys in {:.1?}", fmt_count(keys), t.elapsed());
    let mut index = Index::open(location, &IndexOptions { cache_mb, shards })?;
    index.set_delimiter(b':')?;
    let generation = index.begin_generation(false, Some(keys))?;
    let t = std::time::Instant::now();
    let chunks: Vec<Vec<Vec<u8>>> = all.chunks(merge).map(|c| c.to_vec()).collect();
    drop(all);
    let mut index = if parallel {
        let mut w = index.into_workers()?;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let tc = std::time::Instant::now();
            let n = chunk.len();
            let stats = w.merge(
                chunk.iter().map(Vec::as_slice).collect(),
                generation,
                &keyfold_core::index::ScanSlot::Global,
                &(i as u64 + 1).to_string(),
                ((i + 1) * merge) as u64,
            )?;
            eprintln!(
                "merge {i}: {} keys ({} new) in {:.1?}  rss {}",
                fmt_count(n as u64),
                fmt_count(stats.new),
                tc.elapsed(),
                sysinfo::rss_bytes().map_or("?".into(), fmt_bytes)
            );
        }
        w.join()?
    } else {
        for (i, chunk) in chunks.into_iter().enumerate() {
            let tc = std::time::Instant::now();
            let n = chunk.len();
            let stats = index.ingest(chunk, generation, i as u64 + 1, ((i + 1) * merge) as u64)?;
            eprintln!(
                "merge {i}: {} keys ({} new) in {:.1?}  rss {}",
                fmt_count(n as u64),
                fmt_count(stats.new),
                tc.elapsed(),
                sysinfo::rss_bytes().map_or("?".into(), fmt_bytes)
            );
        }
        index
    };
    let ingest_time = t.elapsed();
    let t = std::time::Instant::now();
    index.complete_generation(generation)?;
    let complete_time = t.elapsed();
    index.checkpoint()?;
    println!(
        "ingested {} keys in {:.1?} ({} keys/s), completion {:.1?}, disk {}, rss {}",
        fmt_count(keys),
        ingest_time,
        fmt_count((keys as f64 / ingest_time.as_secs_f64()) as u64),
        complete_time,
        fmt_bytes(index.disk_size()),
        sysinfo::rss_bytes().map_or("?".into(), fmt_bytes)
    );
    Ok(())
}

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
