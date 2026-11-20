//! The scan engine (CONCEPT section 4): one Tokio task per scanned node
//! that walks `SCAN` with an adaptive `COUNT`, a writer thread that owns
//! the index and persists every node's cursor with each merge, and a
//! coordinator that sums progress and honours pause, resume and stop.
//!
//! A standalone server is a scan of one node. A cluster is one cursor per
//! shard, scanned concurrently into the same index (CONCEPT 4, "Cluster").
//! Replicas are preferred when the target allows ([`targets`]). The
//! gentleness profile sets the gap between calls; the normal profile also
//! backs off when the server's own load or slow log rises ([`LoadMonitor`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use redis::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::capability::{self, Capabilities};
use crate::cluster::{NodeAddr, Role, Topology};
use crate::commands;
use crate::connection::{Conn, ConnectionManager, ScanTarget};
use crate::error::{Error, Result};
use crate::index::workers::Workers;
use crate::index::{Index, KeyBatch, ScanSlot, format_cursors};
use crate::segment::detect_delimiter;

/// Gentleness profile (CONCEPT section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanProfile {
    /// Continuous calls, no gaps.
    Fast,
    /// A small gap between calls, widened while the server's own load or
    /// slow log rises.
    #[default]
    Normal,
    /// Replicas only, 50 ms gaps. Default for prod-tagged connections.
    Polite,
}

impl ScanProfile {
    pub fn gap(self) -> Duration {
        match self {
            ScanProfile::Fast => Duration::ZERO,
            ScanProfile::Normal => Duration::from_millis(2),
            ScanProfile::Polite => Duration::from_millis(50),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScanProfile::Fast => "fast",
            ScanProfile::Normal => "normal",
            ScanProfile::Polite => "polite",
        }
    }

    pub fn parse(s: &str) -> Option<ScanProfile> {
        match s.to_ascii_lowercase().as_str() {
            "fast" => Some(ScanProfile::Fast),
            "normal" => Some(ScanProfile::Normal),
            "polite" => Some(ScanProfile::Polite),
            _ => None,
        }
    }

    /// The profile watches the server's load between calls.
    pub fn monitors_load(self) -> bool {
        self == ScanProfile::Normal
    }
}

/// How `COUNT` adapts to the observed cost of each call.
#[derive(Debug, Clone, Copy)]
pub struct CountTuning {
    pub initial: usize,
    pub min: usize,
    pub max: usize,
    /// Grow while the estimated server time per call stays below this.
    pub grow_below: Duration,
    /// Shrink when it rises above this.
    pub shrink_above: Duration,
}

impl Default for CountTuning {
    fn default() -> Self {
        CountTuning {
            initial: 1000,
            min: 100,
            max: 10_000,
            grow_below: Duration::from_millis(4),
            shrink_above: Duration::from_millis(15),
        }
    }
}

/// Adaptive `COUNT`: server time is estimated as round trip minus the
/// `PING` baseline, smoothed, and compared with the tuning thresholds.
#[derive(Debug, Clone)]
pub struct AdaptiveCount {
    tuning: CountTuning,
    count: usize,
    smoothed: Option<Duration>,
    /// Upper bound imposed by the load monitor's backoff.
    cap: usize,
}

impl AdaptiveCount {
    pub fn new(tuning: CountTuning) -> Self {
        AdaptiveCount {
            count: tuning.initial.clamp(tuning.min, tuning.max),
            cap: tuning.max,
            tuning,
            smoothed: None,
        }
    }

    pub fn count(&self) -> usize {
        self.count.min(self.cap)
    }

    /// Bound `COUNT` from above (backoff); `max` lifts the bound.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.clamp(self.tuning.min, self.tuning.max);
    }

    /// Feed one observation and return the `COUNT` for the next call.
    pub fn observe(&mut self, rtt: Duration, baseline: Duration) -> usize {
        let server = rtt.saturating_sub(baseline);
        let s = match self.smoothed {
            None => server,
            Some(prev) => (prev * 3 + server) / 4,
        };
        self.smoothed = Some(s);
        if s < self.tuning.grow_below {
            self.count = (self.count + self.count / 2).min(self.tuning.max);
        } else if s > self.tuning.shrink_above {
            self.count = (self.count / 2).max(self.tuning.min);
        }
        self.count()
    }
}

/// Backoff policy of the normal profile: the server's foreign load
/// (`instantaneous_ops_per_sec` minus our own call rate) is compared with
/// the level measured before the scan started, and the slow log is watched
/// for growth. Each hot sample doubles the gap and halves the `COUNT` cap,
/// up to [`LoadMonitor::MAX_LEVEL`]; three calm samples in a row step back.
#[derive(Debug, Clone, Default)]
pub struct LoadMonitor {
    baseline_ops: Option<f64>,
    last_ops: Option<f64>,
    last_slowlog: Option<i64>,
    slowlog_grew: bool,
    level: u8,
    calm: u8,
}

impl LoadMonitor {
    pub const MAX_LEVEL: u8 = 6;
    pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn baseline_ops(&self) -> Option<f64> {
        self.baseline_ops
    }

    pub fn last_ops(&self) -> Option<f64> {
        self.last_ops
    }

    /// The first sample, taken before the scan issues its first call.
    pub fn baseline(&mut self, ops: Option<f64>, slowlog: Option<i64>) {
        self.baseline_ops = ops;
        self.last_slowlog = slowlog;
    }

    /// One sample during the scan; `own_rate` is our calls per second
    /// since the previous sample. Returns the new level.
    pub fn observe(&mut self, ops: Option<f64>, own_rate: f64, slowlog: Option<i64>) -> u8 {
        let foreign = ops.map(|o| (o - own_rate).max(0.0));
        self.last_ops = foreign;
        let grew = matches!((self.last_slowlog, slowlog), (Some(prev), Some(now)) if now > prev);
        if slowlog.is_some() {
            self.last_slowlog = slowlog;
        }
        self.slowlog_grew = grew;
        let hot = match (self.baseline_ops, foreign) {
            (Some(base), Some(f)) => f > base * 1.5 + 100.0,
            _ => false,
        };
        if hot || grew {
            self.level = (self.level + 1).min(Self::MAX_LEVEL);
            self.calm = 0;
        } else {
            self.calm += 1;
            if self.calm >= 3 && self.level > 0 {
                self.level -= 1;
                self.calm = 0;
            }
        }
        self.level
    }

    /// The gap in force: the profile's gap doubled per level (and at least
    /// 2 ms once backing off, so a fast profile still yields).
    pub fn gap(&self, base: Duration) -> Duration {
        if self.level == 0 {
            return base;
        }
        base.max(Duration::from_millis(2)) * (1u32 << self.level)
    }

    /// The `COUNT` cap in force.
    pub fn count_cap(&self, max: usize) -> usize {
        max >> self.level
    }

    pub fn note(&self) -> Option<String> {
        if self.level == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if let (Some(b), Some(o)) = (self.baseline_ops, self.last_ops) {
            parts.push(format!("server at {o:.0} ops/s, {b:.0} before the scan"));
        }
        if self.slowlog_grew {
            parts.push("slow log grew".into());
        }
        Some(format!("backing off (level {}): {}", self.level, parts.join(", ")))
    }
}

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub profile: ScanProfile,
    /// Overrides the profile's gap when set.
    pub gap: Option<Duration>,
    /// Start a new generation even if a resumable scan exists.
    pub restart: bool,
    /// `SCAN MATCH`: subtree rescans. Makes the generation partial.
    pub pattern: Option<Vec<u8>>,
    /// `SCAN TYPE`: typed views. Makes the generation partial.
    pub type_filter: Option<String>,
    /// Scan the subtree under this prefix (`SCAN MATCH prefix*`, the prefix
    /// taken literally) with a freshness record of its own: on completion
    /// the index is exact under the prefix and unchanged elsewhere, and the
    /// state of the full scan is not touched. Excludes `pattern` and
    /// `sample`.
    pub subtree: Option<Vec<u8>>,
    /// Stop once about this many keys were read: enough to show the
    /// top-level namespaces and to detect the delimiter. Never prunes,
    /// establishes nothing, is not resumed.
    pub sample: Option<u64>,
    pub tuning: CountTuning,
    /// Batches that may wait for the writer before SCAN calls pause.
    pub queue_depth: usize,
    /// Force a delimiter instead of detecting one from the first keys.
    pub delimiter: Option<u8>,
    /// Keys sampled before the delimiter is chosen; also the size of the
    /// first merge, so the tree fills within the first moments of a scan.
    pub delimiter_sample: usize,
    /// Keys are buffered in memory and merged into the index in sorted
    /// batches. The batch size grows from `delimiter_sample` by a factor of
    /// four per merge until it reaches this many keys ...
    pub merge_max_keys: usize,
    /// ... or this many bytes of key names.
    pub merge_max_bytes: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            profile: ScanProfile::Fast,
            gap: None,
            restart: false,
            pattern: None,
            type_filter: None,
            subtree: None,
            sample: None,
            tuning: CountTuning::default(),
            queue_depth: 8,
            delimiter: None,
            delimiter_sample: 10_000,
            merge_max_keys: 2_000_000,
            merge_max_bytes: 96 << 20,
        }
    }
}

impl ScanConfig {
    pub fn is_partial(&self) -> bool {
        self.pattern.is_some() || self.type_filter.is_some() || self.subtree.is_some() || self.sample.is_some()
    }

    fn gap(&self) -> Duration {
        self.gap.unwrap_or_else(|| self.profile.gap())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanPhase {
    #[default]
    Starting,
    Scanning,
    Paused,
    /// Cursor reached 0; the writer is flushing and pruning.
    Finishing,
    Complete,
    /// Stopped by request; the cursor is persisted for a resume.
    Stopped,
    Failed,
}

/// Progress of one scanned node.
#[derive(Debug, Clone, Default)]
pub struct NodeProgress {
    pub id: String,
    pub label: String,
    pub role: Role,
    pub dbsize: Option<u64>,
    pub scanned: u64,
    pub calls: u64,
    pub count: usize,
    pub cursor: u64,
    /// Sum of `COUNT` over this node's calls: about how many keys the
    /// server walked, which a filtered scan cannot tell from its replies.
    pub examined: u64,
    pub done: bool,
    pub last_rtt: Duration,
    pub backoff: u8,
    pub error: Option<String>,
}

/// Live progress, published after every `SCAN` call on any node.
#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub generation: i64,
    /// The scan continued from persisted cursors; keys created in the
    /// meantime may have been missed.
    pub resumed: bool,
    pub partial: bool,
    /// The prefix of a subtree scan.
    pub subtree: Option<Vec<u8>>,
    /// The number of keys a sample stops at.
    pub sample: Option<u64>,
    /// Sum of `COUNT` over all calls (see [`NodeProgress::examined`]).
    pub examined: u64,
    /// Sum over the scanned nodes.
    pub dbsize: Option<u64>,
    /// Keys returned by SCAN in this generation, duplicates included.
    pub scanned: u64,
    /// Keys the index did not have before this generation.
    pub indexed_new: u64,
    pub calls: u64,
    /// `COUNT` of the last call on any node.
    pub count: usize,
    pub last_rtt: Duration,
    pub baseline_rtt: Duration,
    pub elapsed: Duration,
    /// Batches waiting for the writer thread.
    pub queued: usize,
    pub pruned: u64,
    pub error: Option<String>,
    pub nodes: Vec<NodeProgress>,
    /// Highest backoff level over the nodes (normal profile).
    pub backoff: u8,
    pub backoff_note: Option<String>,
}

impl ScanProgress {
    /// How far the scan is, 0 to 1. A full scan counts the keys it read
    /// against `DBSIZE`; a sample against its target; a filtered scan
    /// (subtree, pattern, type) only sees its matches, so it estimates the
    /// walk from the `COUNT` it asked for. `None` without `DBSIZE`.
    pub fn fraction(&self) -> Option<f64> {
        if matches!(self.phase, ScanPhase::Complete) {
            return Some(1.0);
        }
        if let Some(target) = self.sample {
            return Some((self.scanned as f64 / target.max(1) as f64).min(1.0));
        }
        let done = if self.is_estimate() { self.examined } else { self.scanned };
        let cap = if self.is_estimate() { 0.99 } else { 1.0 };
        self.dbsize.filter(|&n| n > 0).map(|n| (done as f64 / n as f64).min(cap))
    }

    /// [`ScanProgress::fraction`] is an estimate, not a count.
    pub fn is_estimate(&self) -> bool {
        self.partial && self.sample.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Run,
    Pause,
    Stop,
}

/// A node to scan: its connection and how to show it.
pub struct ScanNode {
    pub conn: Conn,
    /// Cluster node id; empty for a standalone server.
    pub id: String,
    pub label: String,
    pub role: Role,
}

impl ScanNode {
    pub fn single(conn: Conn, role: Role) -> ScanNode {
        ScanNode {
            id: String::new(),
            label: conn.label().to_string(),
            role,
            conn,
        }
    }
}

/// Everything the engine returns when the task ends.
#[derive(Debug)]
pub struct ScanOutcome {
    pub index: Index,
    pub progress: ScanProgress,
}

/// Handle to a running scan.
pub struct ScanHandle {
    control: watch::Sender<Control>,
    progress: watch::Receiver<ScanProgress>,
    task: JoinHandle<Result<ScanOutcome>>,
}

impl ScanHandle {
    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }

    pub fn resume(&self) {
        let _ = self.control.send(Control::Run);
    }

    /// Stop after the current call. The cursors stay persisted.
    pub fn stop(&self) {
        let _ = self.control.send(Control::Stop);
    }

    pub fn progress(&self) -> watch::Receiver<ScanProgress> {
        self.progress.clone()
    }

    pub async fn wait(self) -> Result<ScanOutcome> {
        match self.task.await {
            Ok(r) => r,
            Err(e) => Err(Error::Scan(format!("scan task panicked: {e}"))),
        }
    }
}

// ---- choosing the nodes ------------------------------------------------------

/// The nodes a scan will read from, and notes for the user about the
/// choice (a replica that was unreachable, for example).
pub struct TargetPlan {
    pub nodes: Vec<ScanNode>,
    pub notes: Vec<String>,
}

/// Choose and connect the nodes to scan (CONCEPT 4, "prefer replicas").
/// Standalone: the replicas `INFO replication` lists, else the server
/// itself. Cluster: one node per shard, a replica (with `READONLY`) when
/// the target allows and one is reachable, else the primary. The polite
/// profile scans replicas only unless the target says `primary` outright.
pub async fn targets(
    manager: &ConnectionManager,
    caps: &Capabilities,
    topology: Option<&Topology>,
    target: ScanTarget,
    profile: ScanProfile,
) -> Result<TargetPlan> {
    let mut notes = Vec::new();
    let replica_required = target == ScanTarget::Replica || (profile == ScanProfile::Polite && target == ScanTarget::Auto);
    let want_replica = target != ScanTarget::Primary;
    let mut nodes = Vec::new();

    match topology {
        None => {
            if want_replica && !caps.is_replica() {
                for r in caps.replicas.iter().filter(|r| r.online()) {
                    match manager.connect_to(&r.addr).await {
                        Ok(conn) => {
                            nodes.push(ScanNode::single(conn, Role::Replica));
                            break;
                        }
                        Err(e) => notes.push(format!("replica {} unreachable: {e}", r.addr)),
                    }
                }
                if nodes.is_empty() {
                    if replica_required {
                        return Err(Error::Config(format!(
                            "{} scans replicas only and none is reachable{}; choose the primary target to scan it anyway",
                            if profile == ScanProfile::Polite {
                                "the polite profile"
                            } else {
                                "the replica target"
                            },
                            if caps.replicas.is_empty() {
                                " (the server lists no replica)"
                            } else {
                                ""
                            }
                        )));
                    }
                    if want_replica {
                        notes.push(if caps.replicas.is_empty() {
                            "no replica listed; scanning the primary".into()
                        } else {
                            "no replica reachable; scanning the primary".into()
                        });
                    }
                }
            }
            if nodes.is_empty() {
                let role = if caps.is_replica() { Role::Replica } else { Role::Primary };
                let conn = manager.connect().await?;
                nodes.push(ScanNode::single(conn, role));
            }
        }
        Some(t) => {
            for (i, shard) in t.shards.iter().enumerate() {
                if shard.slots.is_empty() {
                    continue;
                }
                let mut chosen = None;
                if want_replica {
                    for r in shard.replicas().filter(|r| r.online) {
                        match manager.connect_to(&r.addr).await {
                            Ok(mut conn) => match conn.raw(&commands::readonly()).await {
                                Ok(_) => {
                                    chosen = Some(ScanNode {
                                        id: r.id.clone(),
                                        label: r.addr.to_string(),
                                        role: Role::Replica,
                                        conn,
                                    });
                                    break;
                                }
                                Err(e) => notes.push(format!("replica {} refused READONLY: {e}", r.addr)),
                            },
                            Err(e) => notes.push(format!("replica {} unreachable: {e}", r.addr)),
                        }
                    }
                }
                if chosen.is_none() {
                    let Some(p) = shard.primary() else {
                        return Err(Error::Scan(format!("shard {i} has no primary")));
                    };
                    if want_replica && replica_required {
                        return Err(Error::Config(format!(
                            "{} scans replicas only and shard {} ({}) has none reachable; choose the primary target to scan the primaries",
                            if profile == ScanProfile::Polite {
                                "the polite profile"
                            } else {
                                "the replica target"
                            },
                            i + 1,
                            p.addr
                        )));
                    }
                    if want_replica {
                        notes.push(format!("shard {}: no replica reachable, scanning primary {}", i + 1, p.addr));
                    }
                    let conn = manager.connect_to(&p.addr).await?;
                    chosen = Some(ScanNode {
                        id: p.id.clone(),
                        label: p.addr.to_string(),
                        role: Role::Primary,
                        conn,
                    });
                }
                nodes.extend(chosen);
            }
            if nodes.is_empty() {
                return Err(Error::Scan("the cluster has no shard with slots".into()));
            }
        }
    }
    Ok(TargetPlan { nodes, notes })
}

/// The addresses a plan would scan, for logs.
pub fn describe(nodes: &[ScanNode]) -> String {
    nodes
        .iter()
        .map(|n| format!("{} ({})", n.label, n.role.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- the engine ---------------------------------------------------------------

enum WriterMsg {
    Batch {
        node: usize,
        keys: Vec<Vec<u8>>,
        cursor: u64,
        scanned: u64,
    },
    Complete,
    Stop,
}

struct Shared {
    indexed_new: AtomicU64,
    queued: AtomicUsize,
    pruned: AtomicU64,
}

enum NodeEvent {
    Progress(usize, NodeProgress),
    /// `Ok(true)`: cursor reached 0; `Ok(false)`: stopped by request.
    Done(usize, Result<bool>),
}

/// Start a scan of one node on its own connection. The index moves to a
/// writer thread for the duration and comes back in the [`ScanOutcome`].
pub fn start(conn: Conn, index: Index, config: ScanConfig) -> ScanHandle {
    start_nodes(vec![ScanNode::single(conn, Role::Primary)], index, config)
}

/// Start a scan of several nodes (one per shard) into one index.
pub fn start_nodes(nodes: Vec<ScanNode>, index: Index, config: ScanConfig) -> ScanHandle {
    let (control_tx, control_rx) = watch::channel(Control::Run);
    let (progress_tx, progress_rx) = watch::channel(ScanProgress::default());
    let task = tokio::spawn(run(nodes, index, config, control_rx, progress_tx));
    ScanHandle {
        control: control_tx,
        progress: progress_rx,
        task,
    }
}

async fn run(
    mut nodes: Vec<ScanNode>,
    mut index: Index,
    mut config: ScanConfig,
    mut control: watch::Receiver<Control>,
    progress_tx: watch::Sender<ScanProgress>,
) -> Result<ScanOutcome> {
    if nodes.is_empty() {
        return Err(Error::Scan("no node to scan".into()));
    }
    // Where this scan keeps its cursors, and what completing it means.
    let slot = match (&config.subtree, config.sample) {
        (Some(_), Some(_)) => return Err(Error::Config("a scan is a subtree scan or a sample, not both".into())),
        (Some(prefix), None) => {
            if config.pattern.is_some() || config.type_filter.is_some() {
                return Err(Error::Config("a subtree scan takes no pattern and no type".into()));
            }
            if prefix.is_empty() {
                return Err(Error::Config("a subtree scan needs a prefix; everything is a full scan".into()));
            }
            let slot = ScanSlot::Subtree(prefix.clone());
            config.pattern = Some(crate::search::Query::prefix(prefix).term);
            slot
        }
        (None, Some(_)) => ScanSlot::None,
        (None, None) => ScanSlot::Global,
    };
    let started = Instant::now();
    let mut progress = ScanProgress {
        partial: config.is_partial(),
        subtree: config.subtree.clone(),
        sample: config.sample,
        nodes: nodes
            .iter()
            .map(|n| NodeProgress {
                id: n.id.clone(),
                label: n.label.clone(),
                role: n.role,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };

    // DBSIZE per node may be denied; the progress bar then falls back to counts only.
    let mut dbsize_total: Option<u64> = None;
    for (i, n) in nodes.iter_mut().enumerate() {
        let d = match n.conn.raw(&commands::dbsize()).await {
            Ok(Value::Int(n)) if n >= 0 => Some(n as u64),
            Ok(_) => None,
            Err(e) if e.is_denied() => None,
            Err(e) => return Err(e),
        };
        progress.nodes[i].dbsize = d;
        if let Some(d) = d {
            dbsize_total = Some(dbsize_total.unwrap_or(0) + d);
        }
    }
    progress.dbsize = dbsize_total;

    // Generation: resume a running scan with the same shape on the same
    // nodes, else start a new one.
    let mut ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    ids.sort();
    let same_nodes = |stored: &[(String, u64)]| {
        let mut stored_ids: Vec<&str> = stored.iter().map(|(id, _)| id.as_str()).collect();
        stored_ids.sort();
        stored_ids == ids.iter().map(String::as_str).collect::<Vec<_>>()
    };
    let mut cursors = vec![0u64; nodes.len()];
    match &slot {
        ScanSlot::Subtree(prefix) => match index.subtree_run(prefix)?.filter(|r| !config.restart && same_nodes(&r.cursors)) {
            Some(r) => {
                progress.generation = r.generation;
                progress.resumed = true;
                progress.scanned = r.scanned;
                for (i, n) in nodes.iter().enumerate() {
                    cursors[i] = r.cursors.iter().find(|(id, _)| *id == n.id).map(|(_, c)| *c).unwrap_or(0);
                    progress.nodes[i].cursor = cursors[i];
                }
                tracing::info!(generation = r.generation, ?cursors, "resuming subtree scan from persisted cursors");
            }
            None => progress.generation = index.begin_subtree(prefix)?,
        },
        ScanSlot::None => progress.generation = index.next_generation()?,
        ScanSlot::Global => {
            let state = index.scan_state()?;
            let stored_pattern = index.meta_text("scan.pattern")?.filter(|s| !s.is_empty());
            let stored_type = index.meta_text("scan.type")?.filter(|s| !s.is_empty());
            let same_shape = stored_pattern.as_deref() == config.pattern.as_ref().map(|p| String::from_utf8_lossy(p)).as_deref()
                && stored_type.as_deref() == config.type_filter.as_deref();
            if !config.restart && state.resumable() && same_shape && same_nodes(&state.cursors) {
                progress.generation = state.generation;
                progress.resumed = true;
                progress.scanned = state.scanned;
                for (i, n) in nodes.iter().enumerate() {
                    cursors[i] = state.cursors.iter().find(|(id, _)| *id == n.id).map(|(_, c)| *c).unwrap_or(0);
                    progress.nodes[i].cursor = cursors[i];
                }
                index.mark_running()?;
                tracing::info!(generation = state.generation, ?cursors, "resuming scan from persisted cursors");
            } else {
                progress.generation = index.begin_generation(config.is_partial(), progress.dbsize)?;
                match &config.pattern {
                    Some(p) => index.set_meta_text("scan.pattern", &String::from_utf8_lossy(p))?,
                    None => index.set_meta_text("scan.pattern", "")?,
                }
                index.set_meta_text("scan.type", config.type_filter.as_deref().unwrap_or(""))?;
            }
        }
    }
    if let Some(d) = config.delimiter {
        match index.delimiter() {
            Some(cur) if cur != d => index.rebuild_nodes(d)?,
            Some(_) => {}
            None => index.set_delimiter(d)?,
        }
    }

    // Baseline round trip per node for the adaptive COUNT.
    let mut baselines = Vec::with_capacity(nodes.len());
    for n in nodes.iter_mut() {
        let mut baseline = Duration::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            n.conn.raw(&commands::ping()).await?;
            baseline = baseline.min(t.elapsed());
        }
        baselines.push(baseline);
    }
    progress.baseline_rtt = baselines.iter().copied().max().unwrap_or_default();

    let shared = Arc::new(Shared {
        indexed_new: AtomicU64::new(0),
        queued: AtomicUsize::new(0),
        pruned: AtomicU64::new(0),
    });
    let (tx, rx) = mpsc::channel::<WriterMsg>(config.queue_depth.max(1));
    let writer = {
        let shared = Arc::clone(&shared);
        let generation = progress.generation;
        let merge = MergePolicy {
            first: config.delimiter_sample.max(1),
            max_keys: config.merge_max_keys.max(1),
            max_bytes: config.merge_max_bytes,
        };
        let node_ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let initial = NodeCursors {
            cursors: cursors.clone(),
            scanned: progress.nodes.iter().map(|n| n.scanned).collect(),
        };
        thread::Builder::new()
            .name("index-writer".into())
            .spawn(move || writer_loop(index, rx, (generation, slot), merge, shared, node_ids, initial))
            .map_err(Error::Io)?
    };

    // One task per node; they watch an internal control so the coordinator
    // can stop them all when one fails.
    let (ictl_tx, ictl_rx) = watch::channel(Control::Run);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<NodeEvent>();
    let mut running = 0usize;
    for (i, node) in nodes.into_iter().enumerate() {
        let task = NodeTask {
            idx: i,
            conn: node.conn,
            cursor: cursors[i],
            baseline: baselines[i],
            config: config.clone(),
            control: ictl_rx.clone(),
            tx: tx.clone(),
            events: events_tx.clone(),
            progress: progress.nodes[i].clone(),
            shared: Arc::clone(&shared),
        };
        tokio::spawn(task.run());
        running += 1;
    }
    drop(events_tx);
    drop(ictl_rx);

    progress.count = config.tuning.initial;
    progress.phase = ScanPhase::Scanning;
    let _ = progress_tx.send_replace(progress.clone());

    let mut outcome_phase = ScanPhase::Complete;
    let mut failure: Option<Error> = None;
    let mut stopped = false;
    // a sample that has read enough stops its nodes and is complete
    let mut sampled = false;
    while running > 0 {
        tokio::select! {
            changed = control.changed() => {
                let c = if changed.is_err() { Control::Stop } else { *control.borrow() };
                let _ = ictl_tx.send(c);
                progress.phase = match c {
                    Control::Pause => ScanPhase::Paused,
                    _ => ScanPhase::Scanning,
                };
                progress.elapsed = started.elapsed();
                let _ = progress_tx.send_replace(progress.clone());
            }
            ev = events_rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    NodeEvent::Progress(i, np) => {
                        progress.count = np.count;
                        progress.last_rtt = np.last_rtt;
                        progress.nodes[i] = np;
                    }
                    NodeEvent::Done(i, result) => {
                        running -= 1;
                        progress.nodes[i].done = true;
                        match result {
                            Ok(true) => {}
                            Ok(false) => stopped = true,
                            Err(e) => {
                                progress.nodes[i].error = Some(e.to_string());
                                if failure.is_none() {
                                    failure = Some(e);
                                }
                                let _ = ictl_tx.send(Control::Stop);
                            }
                        }
                    }
                }
                progress.scanned = progress.nodes.iter().map(|n| n.scanned).sum();
                progress.examined = progress.nodes.iter().map(|n| n.examined).sum();
                progress.calls = progress.nodes.iter().map(|n| n.calls).sum();
                if !sampled && config.sample.is_some_and(|n| progress.scanned >= n) {
                    sampled = true;
                    let _ = ictl_tx.send(Control::Stop);
                }
                progress.backoff = progress.nodes.iter().map(|n| n.backoff).max().unwrap_or(0);
                progress.indexed_new = shared.indexed_new.load(Ordering::Relaxed);
                progress.queued = shared.queued.load(Ordering::Relaxed);
                progress.elapsed = started.elapsed();
                let _ = progress_tx.send_replace(progress.clone());
            }
        }
    }
    drop(ictl_tx);
    if failure.is_some() {
        outcome_phase = ScanPhase::Failed;
    } else if stopped && !sampled {
        outcome_phase = ScanPhase::Stopped;
    }

    if outcome_phase == ScanPhase::Complete && !(sampled && stopped) {
        progress.phase = ScanPhase::Finishing;
        let _ = progress_tx.send_replace(progress.clone());
        let _ = tx.send(WriterMsg::Complete).await;
    } else {
        let _ = tx.send(WriterMsg::Stop).await;
    }
    drop(tx);

    let index = match tokio::task::spawn_blocking(move || writer.join()).await {
        Ok(Ok(Ok(index))) => index,
        Ok(Ok(Err(e))) => return Err(failure.unwrap_or(e)),
        Ok(Err(_)) => return Err(Error::Scan("index writer panicked".into())),
        Err(e) => return Err(Error::Scan(format!("join: {e}"))),
    };

    progress.indexed_new = shared.indexed_new.load(Ordering::Relaxed);
    progress.pruned = shared.pruned.load(Ordering::Relaxed);
    progress.queued = 0;
    progress.elapsed = started.elapsed();
    progress.phase = outcome_phase;
    progress.error = failure.as_ref().map(|e| e.to_string());
    let _ = progress_tx.send_replace(progress.clone());

    match failure {
        Some(e) => Err(e),
        None => Ok(ScanOutcome { index, progress }),
    }
}

/// The `SCAN` loop of one node.
struct NodeTask {
    idx: usize,
    conn: Conn,
    cursor: u64,
    baseline: Duration,
    config: ScanConfig,
    control: watch::Receiver<Control>,
    tx: mpsc::Sender<WriterMsg>,
    events: mpsc::UnboundedSender<NodeEvent>,
    progress: NodeProgress,
    shared: Arc<Shared>,
}

impl NodeTask {
    async fn run(mut self) {
        let result = self.scan().await;
        let _ = self.events.send(NodeEvent::Done(self.idx, result));
    }

    async fn scan(&mut self) -> Result<bool> {
        let mut adaptive = AdaptiveCount::new(self.config.tuning);
        self.progress.count = adaptive.count();
        let base_gap = self.config.gap();
        let mut monitor = self.config.profile.monitors_load().then(LoadMonitor::default);
        let mut last_sample = Instant::now();
        let mut calls_at_sample = 0u64;
        if let Some(m) = &mut monitor {
            let (ops, slowlog) = sample_load(&mut self.conn).await;
            m.baseline(ops, slowlog);
        }

        loop {
            // Honour pause and stop between calls.
            loop {
                let current = *self.control.borrow_and_update();
                match current {
                    Control::Run => break,
                    Control::Stop => return Ok(false),
                    Control::Pause => {
                        if self.control.changed().await.is_err() {
                            return Ok(false);
                        }
                    }
                }
            }

            let count = adaptive.count();
            let cmd = commands::scan(
                self.cursor,
                self.config.pattern.as_deref(),
                count,
                self.config.type_filter.as_deref(),
            );
            let t = Instant::now();
            let reply = self.conn.raw(&cmd).await?;
            let rtt = t.elapsed();
            let (next, keys) = parse_scan_reply(reply)?;

            self.progress.calls += 1;
            self.progress.examined += count as u64;
            self.progress.scanned += keys.len() as u64;
            self.progress.last_rtt = rtt;
            self.progress.count = adaptive.observe(rtt, self.baseline);
            self.cursor = next;
            self.progress.cursor = next;

            self.shared.queued.fetch_add(1, Ordering::Relaxed);
            if self
                .tx
                .send(WriterMsg::Batch {
                    node: self.idx,
                    keys,
                    cursor: next,
                    scanned: self.progress.scanned,
                })
                .await
                .is_err()
            {
                return Err(Error::Scan("index writer stopped".into()));
            }

            // The normal profile watches the server's load.
            let mut gap = base_gap;
            if let Some(m) = &mut monitor {
                if last_sample.elapsed() >= LoadMonitor::SAMPLE_EVERY {
                    let (ops, slowlog) = sample_load(&mut self.conn).await;
                    let own_rate = (self.progress.calls - calls_at_sample) as f64 / last_sample.elapsed().as_secs_f64().max(0.001);
                    let level = m.observe(ops, own_rate, slowlog);
                    adaptive.set_cap(m.count_cap(self.config.tuning.max));
                    if level != self.progress.backoff {
                        tracing::info!(node = %self.progress.label, level, "scan backoff changed");
                    }
                    self.progress.backoff = level;
                    last_sample = Instant::now();
                    calls_at_sample = self.progress.calls;
                }
                gap = m.gap(base_gap);
            }
            let _ = self.events.send(NodeEvent::Progress(self.idx, self.progress.clone()));

            if self.cursor == 0 {
                return Ok(true);
            }
            if !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
        }
    }
}

/// `INFO stats` (`instantaneous_ops_per_sec`) and `SLOWLOG LEN` in one
/// pipeline; either may be denied and is then `None`.
async fn sample_load(conn: &mut Conn) -> (Option<f64>, Option<i64>) {
    let mut pipe = commands::pipeline();
    pipe.add_command(commands::info("stats"));
    pipe.add_command(commands::slowlog_len());
    pipe.ignore_errors();
    let Ok(replies) = conn.pipeline(&pipe).await else {
        return (None, None);
    };
    let ops = replies
        .first()
        .and_then(|r| r.as_ref().ok())
        .map(|v| capability::parse_info(&capability::value_text(v)))
        .and_then(|info| info.get("instantaneous_ops_per_sec").and_then(|s| s.parse().ok()));
    let slowlog = match replies.get(1) {
        Some(Ok(Value::Int(n))) => Some(*n),
        _ => None,
    };
    (ops, slowlog)
}

/// `SCAN` replies with `[cursor, [key, ...]]` in both RESP2 and RESP3.
pub fn parse_scan_reply(v: Value) -> Result<(u64, Vec<Vec<u8>>)> {
    let mut items = match v {
        Value::Array(items) if items.len() == 2 => items.into_iter(),
        other => return Err(Error::reply("SCAN", format!("expected a 2-element array, got {other:?}"))),
    };
    let cursor = match items.next() {
        Some(Value::BulkString(b)) => std::str::from_utf8(&b).ok().and_then(|s| s.parse::<u64>().ok()),
        Some(Value::SimpleString(s)) => s.parse::<u64>().ok(),
        Some(Value::Int(i)) if i >= 0 => Some(i as u64),
        _ => None,
    }
    .ok_or_else(|| Error::reply("SCAN", "cursor is not an unsigned integer"))?;
    let keys = match items.next() {
        Some(Value::Array(ks)) => ks
            .into_iter()
            .map(|k| match k {
                Value::BulkString(b) => Ok(b),
                Value::SimpleString(s) => Ok(s.into_bytes()),
                other => Err(Error::reply("SCAN", format!("key is not a string: {other:?}"))),
            })
            .collect::<Result<Vec<_>>>()?,
        other => return Err(Error::reply("SCAN", format!("keys are not an array: {other:?}"))),
    };
    Ok((cursor, keys))
}

#[derive(Debug, Clone, Copy)]
struct MergePolicy {
    first: usize,
    max_keys: usize,
    max_bytes: usize,
}

/// Per-node cursors as of the last batch merged.
struct NodeCursors {
    cursors: Vec<u64>,
    scanned: Vec<u64>,
}

/// Keys waiting to be merged, with the cursor of every node that follows
/// the last batch they came from. Persisting those cursors with the merge
/// means a resume never skips keys that were only buffered.
struct MergeBuffer {
    keys: KeyBatch,
    cursors: NodeCursors,
    threshold: usize,
}

impl MergeBuffer {
    fn push(&mut self, node: usize, keys: Vec<Vec<u8>>, cursor: u64, scanned: u64) {
        for k in &keys {
            self.keys.push(k);
        }
        self.cursors.cursors[node] = cursor;
        self.cursors.scanned[node] = scanned;
    }

    fn is_due(&self, policy: &MergePolicy) -> bool {
        self.keys.len() >= self.threshold || self.keys.bytes() >= policy.max_bytes
    }

    fn merge(
        &mut self,
        workers: &mut Workers,
        generation: i64,
        slot: &ScanSlot,
        policy: &MergePolicy,
        shared: &Shared,
        ids: &[String],
    ) -> Result<()> {
        if workers.delimiter().is_none() {
            let d = detect_delimiter((0..self.keys.len() as u32).map(|i| self.keys.key(i)));
            tracing::info!(delimiter = %(d as char), sampled = self.keys.len(), "delimiter detected");
            workers.set_delimiter(d)?;
        }
        if self.keys.is_empty() {
            return Ok(());
        }
        let next = (self.threshold * 4).min(policy.max_keys);
        let batch = std::mem::replace(&mut self.keys, KeyBatch::with_capacity(next, next * 48));
        let cursors: Vec<(String, u64)> = ids.iter().cloned().zip(self.cursors.cursors.iter().copied()).collect();
        let scanned: u64 = self.cursors.scanned.iter().sum();
        let stats = workers.merge(batch, generation, slot, &format_cursors(&cursors), scanned)?;
        shared.indexed_new.fetch_add(stats.new, Ordering::Relaxed);
        self.threshold = next;
        Ok(())
    }
}

fn writer_loop(
    index: Index,
    mut rx: mpsc::Receiver<WriterMsg>,
    (generation, slot): (i64, ScanSlot),
    policy: MergePolicy,
    shared: Arc<Shared>,
    ids: Vec<String>,
    initial: NodeCursors,
) -> Result<Index> {
    let mut workers = index.into_workers()?;
    let mut buffer = MergeBuffer {
        keys: KeyBatch::with_capacity(policy.first, policy.first * 48),
        cursors: initial,
        threshold: policy.first,
    };
    let mut complete = false;
    let outcome = (|| -> Result<()> {
        while let Some(msg) = rx.blocking_recv() {
            match msg {
                WriterMsg::Batch {
                    node,
                    keys,
                    cursor,
                    scanned,
                } => {
                    buffer.push(node, keys, cursor, scanned);
                    shared.queued.fetch_sub(1, Ordering::Relaxed);
                    if buffer.is_due(&policy) {
                        buffer.merge(&mut workers, generation, &slot, &policy, &shared, &ids)?;
                    }
                }
                WriterMsg::Complete => {
                    buffer.merge(&mut workers, generation, &slot, &policy, &shared, &ids)?;
                    complete = true;
                    break;
                }
                WriterMsg::Stop => {
                    buffer.merge(&mut workers, generation, &slot, &policy, &shared, &ids)?;
                    break;
                }
            }
        }
        Ok(())
    })();
    let mut index = workers.join()?;
    outcome?;
    if complete {
        let pruned = match &slot {
            ScanSlot::Global => index.complete_generation(generation)?,
            ScanSlot::Subtree(prefix) => index.complete_subtree(prefix, generation)?,
            // a sample that reached the end of a small database establishes nothing either
            ScanSlot::None => 0,
        };
        shared.pruned.store(pruned, Ordering::Relaxed);
    }
    Ok(index)
}

/// Where a scan may read from, for the UI: the nodes known to the
/// capability layer or the topology, with their roles.
pub fn known_nodes(caps: &Capabilities, topology: Option<&Topology>, seed: &NodeAddr) -> Vec<(NodeAddr, Role, bool)> {
    match topology {
        Some(t) => t.nodes().map(|n| (n.addr.clone(), n.role, n.online)).collect(),
        None => {
            let mut out = vec![(seed.clone(), if caps.is_replica() { Role::Replica } else { Role::Primary }, true)];
            out.extend(caps.replicas.iter().map(|r| (r.addr.clone(), Role::Replica, r.online())));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_grows_when_cheap_and_shrinks_when_slow() {
        let mut a = AdaptiveCount::new(CountTuning::default());
        assert_eq!(a.count(), 1000);
        let base = Duration::from_micros(200);
        for _ in 0..10 {
            a.observe(Duration::from_millis(1), base);
        }
        assert_eq!(a.count(), 10_000);
        for _ in 0..10 {
            a.observe(Duration::from_millis(80), base);
        }
        assert_eq!(a.count(), 100);
        // in the dead band nothing moves
        let before = a.count();
        a.observe(Duration::from_millis(8), base);
        a.observe(Duration::from_millis(8), base);
        a.observe(Duration::from_millis(8), base);
        a.observe(Duration::from_millis(8), base);
        assert_eq!(a.count(), before);
        // a cap bounds the count without losing the learned value
        for _ in 0..40 {
            a.observe(Duration::from_millis(1), base);
        }
        assert_eq!(a.count(), 10_000);
        a.set_cap(2500);
        assert_eq!(a.count(), 2500);
        a.set_cap(usize::MAX);
        assert_eq!(a.count(), 10_000);
    }

    #[test]
    fn load_monitor_backs_off_on_foreign_load_and_slow_log() {
        let mut m = LoadMonitor::default();
        m.baseline(Some(1000.0), Some(10));
        // our own calls do not count as load
        assert_eq!(m.observe(Some(1400.0), 500.0, Some(10)), 0);
        // foreign load up by more than half: back off
        assert_eq!(m.observe(Some(2000.0), 100.0, Some(10)), 1);
        assert_eq!(m.observe(Some(2000.0), 100.0, Some(10)), 2);
        assert_eq!(m.gap(Duration::from_millis(2)), Duration::from_millis(8));
        assert_eq!(m.count_cap(10_000), 2500);
        // slow log growth alone backs off too
        assert_eq!(m.observe(Some(1000.0), 100.0, Some(11)), 3);
        assert!(m.note().unwrap().contains("slow log grew"));
        // three calm samples step back one level
        assert_eq!(m.observe(Some(1000.0), 100.0, Some(11)), 3);
        assert_eq!(m.observe(Some(1000.0), 100.0, Some(11)), 3);
        assert_eq!(m.observe(Some(1000.0), 100.0, Some(11)), 2);
        // without INFO the monitor never backs off on load
        let mut blind = LoadMonitor::default();
        blind.baseline(None, None);
        assert_eq!(blind.observe(None, 100.0, None), 0);
        assert_eq!(blind.gap(Duration::ZERO), Duration::ZERO);
        // the fast profile still yields once backing off
        let mut fast = LoadMonitor::default();
        fast.baseline(Some(0.0), None);
        fast.observe(Some(500.0), 0.0, None);
        assert_eq!(fast.gap(Duration::ZERO), Duration::from_millis(4));
    }

    #[test]
    fn scan_reply_parsing() {
        let v = Value::Array(vec![
            Value::BulkString(b"17".to_vec()),
            Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::BulkString(b"b".to_vec())]),
        ]);
        let (c, k) = parse_scan_reply(v).unwrap();
        assert_eq!(c, 17);
        assert_eq!(k, vec![b"a".to_vec(), b"b".to_vec()]);
        let bad = Value::Array(vec![Value::BulkString(b"x".to_vec()), Value::Array(vec![])]);
        assert!(parse_scan_reply(bad).is_err());
        assert!(parse_scan_reply(Value::Nil).is_err());
    }

    #[test]
    fn progress_fraction() {
        let mut p = ScanProgress {
            dbsize: Some(100),
            scanned: 25,
            ..Default::default()
        };
        assert_eq!(p.fraction(), Some(0.25));
        p.scanned = 300;
        assert_eq!(p.fraction(), Some(1.0));
        p.dbsize = None;
        assert_eq!(p.fraction(), None);
    }
}
