//! One open connection: the server side, the index on disk, the running scan
//! and the read-only index the UI windows come from.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keyfold_core::browse::Pager;
use keyfold_core::index::{Anchors, Index, IndexOptions, Location, ScanStatus};
use keyfold_core::mutate::{self, DeleteHandle, IndexChange, Outcome};
use keyfold_core::notify::{LiveHandle, LiveStatus};
use keyfold_core::scan::{self, ScanConfig, ScanHandle, ScanPhase, ScanProfile, ScanProgress};
use keyfold_core::{Capabilities, ConnectionManager, IndexMode, Profile, Router, ScanTarget, Tag};
use rusqlite::InterruptHandle;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::live::{LiveStatusDto, NotifySettingDto};
use crate::wire::KeyDto;

/// Databases above this many keys are not indexed in full without asking
/// (CONCEPT section 3.2); a profile that names no mode gets the on-demand
/// index there.
pub const AUTO_INDEX_THRESHOLD: u64 = 20_000_000;

/// Keys the first scan of an on-demand index reads to seed the tree.
pub const SAMPLE_KEYS: u64 = 20_000;

/// What a scan is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanJob {
    /// Everything: the full index, or "scan everything" on demand.
    Full,
    /// The first keys of an on-demand index.
    Sample,
    /// One subtree, by its prefix.
    Subtree(Vec<u8>),
}

impl ScanJob {
    pub fn name(&self) -> &'static str {
        match self {
            ScanJob::Full => "full",
            ScanJob::Sample => "sample",
            ScanJob::Subtree(_) => "subtree",
        }
    }
}

/// How the index stands for the keys under one prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixState {
    /// When a completed scan last saw every key under the prefix.
    pub scanned_at: Option<i64>,
    /// `idle`, `queued` or `running`: a scan that will make it fresh.
    pub scan: &'static str,
    /// Counts under the prefix are exact right now.
    pub exact: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub keys: i64,
    pub status: &'static str,
    pub generation: i64,
    pub scanned_at: Option<i64>,
    /// When the whole keyspace was last scanned to the end.
    pub full_at: Option<i64>,
    /// Subtrees with a completed scan of their own.
    pub subtrees: usize,
    pub ephemeral: bool,
    pub path: String,
}

/// A node the connection knows: the server itself and its replicas, or
/// every cluster node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDto {
    pub addr: String,
    /// `primary` or `replica`
    pub role: &'static str,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterDto {
    pub shards: usize,
    pub nodes: usize,
    pub unassigned_slots: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionDto {
    pub id: u32,
    pub label: String,
    /// dev, staging or prod; prod confirms every write.
    pub tag: &'static str,
    pub server: String,
    pub version: String,
    pub mode: String,
    pub role: String,
    pub protocol: u8,
    pub db: i64,
    /// Summed over the primaries of a cluster.
    pub dbsize: Option<u64>,
    pub used_memory: Option<u64>,
    /// Commands the server's ACL refused in this session (`NOPERM`), in
    /// the notation of `COMMAND INFO` (`unlink`, `memory|usage`).
    pub denied: Vec<String>,
    pub index: IndexInfo,
    /// `full`, `onDemand` or `none`
    pub index_mode: &'static str,
    /// The mode was chosen by the database's size, not by the profile.
    pub index_mode_auto: bool,
    pub scanning: bool,
    /// Subtree scans waiting for the running one.
    pub scan_queue: usize,
    /// A console command may have changed keys the index has not seen.
    pub index_stale: bool,
    pub nodes: Vec<NodeDto>,
    pub cluster: Option<ClusterDto>,
    /// Why a cluster is browsed as one node, when it is.
    pub note: Option<String>,
    /// `fast`, `normal` or `polite`
    pub scan_profile: &'static str,
    /// `auto`, `primary` or `replica`
    pub scan_target: &'static str,
    /// What the last scan start said about its node choice.
    pub scan_notes: Vec<String>,
    pub live: LiveStatusDto,
    pub notify: Option<NotifySettingDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressDto {
    pub session: u32,
    /// `full`, `sample` or `subtree`
    pub job: &'static str,
    /// The prefix of a subtree scan.
    pub subtree: Option<KeyDto>,
    /// How far the scan is, 0 to 1, when that can be told.
    pub fraction: Option<f64>,
    /// `fraction` is estimated from the walk, not counted.
    pub estimate: bool,
    /// Subtree scans waiting for this one.
    pub scan_queue: usize,
    pub phase: &'static str,
    pub generation: i64,
    pub resumed: bool,
    pub partial: bool,
    pub dbsize: Option<u64>,
    pub scanned: u64,
    pub indexed_new: u64,
    pub calls: u64,
    pub count: usize,
    pub last_rtt_ms: f64,
    pub elapsed_ms: f64,
    pub queued: usize,
    pub pruned: u64,
    pub index_keys: i64,
    pub error: Option<String>,
    pub nodes: Vec<NodeProgressDto>,
    /// Highest backoff level over the nodes (normal profile).
    pub backoff: u8,
    pub scan_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeProgressDto {
    pub label: String,
    pub role: &'static str,
    pub dbsize: Option<u64>,
    pub scanned: u64,
    pub calls: u64,
    pub count: usize,
    pub done: bool,
    pub backoff: u8,
    pub error: Option<String>,
}

fn phase_name(p: ScanPhase) -> &'static str {
    match p {
        ScanPhase::Starting => "starting",
        ScanPhase::Scanning => "scanning",
        ScanPhase::Paused => "paused",
        ScanPhase::Finishing => "finishing",
        ScanPhase::Complete => "complete",
        ScanPhase::Stopped => "stopped",
        ScanPhase::Failed => "failed",
    }
}

fn status_name(s: ScanStatus) -> &'static str {
    match s {
        ScanStatus::Idle => "idle",
        ScanStatus::Running => "running",
        ScanStatus::Complete => "complete",
    }
}

/// A fold computation and the subtree count it was based on.
type CachedFold = (i64, Arc<serde_json::Value>);

pub struct Session {
    pub id: u32,
    pub profile: Profile,
    pub manager: ConnectionManager,
    pub caps: Capabilities,
    pub location: Location,
    ephemeral_dir: Option<PathBuf>,
    /// Connections for the UI's own commands (metadata, values, writes):
    /// one per node, routed by slot on a cluster.
    pub router: tokio::sync::Mutex<Router>,
    /// Gentleness profile and replica preference for the next scan.
    pub scan_options: Mutex<(ScanProfile, ScanTarget)>,
    pub scan_notes: Mutex<Vec<String>>,
    /// The keyspace-notification subscription, when live updates are on.
    pub live: tokio::sync::Mutex<Option<LiveHandle>>,
    pub live_on: AtomicBool,
    pub live_last: Mutex<LiveStatus>,
    /// Index rows added and removed by notifications in this session.
    pub live_applied: Mutex<(u64, u64)>,
    /// The key the value panel shows (the last one inspected). A live
    /// event for it is passed on as `value:changed`.
    pub open_key: Mutex<Option<Vec<u8>>>,
    /// Read-only view the list windows are served from.
    pub reader: Mutex<Index>,
    /// A second read-only view, used only to rebuild anchors so that the
    /// list never waits behind a rebuild.
    anchor_reader: Mutex<Index>,
    anchors: Mutex<Option<Arc<Anchors>>>,
    anchors_building: AtomicBool,
    /// The writable index while no scan runs; the scan engine owns it otherwise.
    writer: tokio::sync::Mutex<Option<Index>>,
    scan: tokio::sync::Mutex<Option<ScanHandle>>,
    pub last_progress: Mutex<Option<ProgressDto>>,
    pub index_mode: IndexMode,
    pub index_mode_auto: bool,
    /// What the running scan is for.
    job: Mutex<Option<ScanJob>>,
    /// Subtrees to scan once the running scan has ended, in order.
    queue: Mutex<VecDeque<Vec<u8>>>,
    /// The walk of a connection without an index, and the pattern it runs with.
    pub pager: tokio::sync::Mutex<Option<(Pager, String)>>,
    /// The command console's own connections, opened on its first line.
    pub console: tokio::sync::Mutex<Option<keyfold_core::console::Console>>,
    /// A console command may have changed keys the index has not seen;
    /// cleared when a scan of everything completes.
    pub index_stale: AtomicBool,
    /// Fold computations by the level's prefix bytes; dropped when the
    /// subtree count drifts more than 5 %.
    pub fold_cache: Mutex<HashMap<Vec<u8>, CachedFold>>,
    /// Per-session fold rules (force / forbid), keyed like the cache.
    pub fold_rules: Mutex<HashMap<Vec<u8>, keyfold_core::fold::Override>>,
    /// A third read-only view for searches, so a slow substring scan never
    /// holds up list or tree windows.
    pub search_reader: Mutex<Index>,
    search_interrupt: InterruptHandle,
    /// Flag of the search in flight; replaced by every new search and
    /// raised (with an interrupt) to cancel the previous one.
    search_cancel: Mutex<Arc<AtomicBool>>,
    /// Raised to abort a trigram table build.
    pub fts_cancel: Arc<AtomicBool>,
    /// Write-through outcomes that arrived while a scan (or another
    /// operation) owned the writer; applied when it comes back.
    pending: Mutex<Vec<Outcome>>,
    /// The pattern delete in flight, if any; it owns the writer meanwhile.
    pub delete: tokio::sync::Mutex<Option<DeleteHandle>>,
}

impl Session {
    pub async fn open(id: u32, url: &str, tag: &str, index_mode: Option<IndexMode>, cache_dir: PathBuf) -> Result<Session, String> {
        let mut profile = Profile::from_url(url).map_err(|e| e.to_string())?;
        profile.tag = Tag::parse(tag).ok_or_else(|| format!("unknown tag {tag:?}"))?;
        profile.index_mode = index_mode;
        Session::open_profile(id, profile, cache_dir, None).await
    }

    /// Open a session for a profile: `connect` builds one from a URL,
    /// switching the database reuses the one of the session it replaces.
    ///
    /// `tunnel` is the SSH tunnel of the session this one replaces (a
    /// database or index-mode switch), so the same server is not logged
    /// into twice; otherwise a profile with a bastion opens its own.
    pub async fn open_profile(
        id: u32,
        profile: Profile,
        cache_dir: PathBuf,
        tunnel: Option<Arc<keyfold_core::tunnel::Tunnel>>,
    ) -> Result<Session, String> {
        let mut manager = ConnectionManager::new(profile.clone());
        if let Some(t) = tunnel.filter(|t| profile.ssh.as_ref() == Some(t.config())) {
            manager = manager.with_tunnel(t);
        }
        let mut conn = manager.connect().await.map_err(connect_error)?;
        let caps = keyfold_core::capability::probe(&mut conn, profile.db)
            .await
            .map_err(|e| e.to_string())?;
        let router = Router::new(manager.clone(), conn, &caps).await;
        if let Some(note) = router.note() {
            tracing::warn!(id, note, "cluster topology");
        }
        let scan_options = (profile.scan_profile(), profile.scan_target);

        let (index_mode, index_mode_auto) = match profile.index_mode {
            Some(m) => (m, false),
            None if caps.dbsize.is_some_and(|n| n > AUTO_INDEX_THRESHOLD) => (IndexMode::OnDemand, true),
            None => (IndexMode::Full, true),
        };
        // without an index the files still exist, empty, in the temp
        // directory: nothing is ever written to them
        let (location, ephemeral_dir) = if profile.ephemeral_index() || index_mode == IndexMode::None {
            // a directory of its own with a random name, user-only: a
            // name another local user can guess could be put there first
            let mut builder = tempfile::Builder::new();
            builder.prefix("keyfold-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let dir = builder
                .tempdir()
                .map_err(|e| format!("could not create a temporary index directory: {e}"))?
                .keep();
            (Location::File(dir.join("db.sqlite")), Some(dir))
        } else {
            let root = cache_dir.join("indexes");
            let dir = root.join(&profile.id);
            // folder names carry host names, the files key names (CONCEPT 3.4)
            for d in [&root, &dir] {
                keyfold_core::private::own_dir(d).map_err(|e| format!("could not create {}: {e}", d.display()))?;
            }
            (Location::File(dir.join(format!("db{}.sqlite", profile.db))), None)
        };
        let options = IndexOptions::default();
        let (writer, reader, anchor_reader, search_reader) = {
            let location = location.clone();
            tokio::task::spawn_blocking(move || -> Result<_, String> {
                let w = Index::open(location.clone(), &options).map_err(|e| e.to_string())?;
                let r = Index::open_reader(&location, &options).map_err(|e| e.to_string())?;
                let a = Index::open_reader(&location, &IndexOptions { cache_mb: 16, ..options }).map_err(|e| e.to_string())?;
                let s = Index::open_reader(&location, &IndexOptions { cache_mb: 32, ..options }).map_err(|e| e.to_string())?;
                Ok((w, r, a, s))
            })
            .await
            .map_err(|e| e.to_string())??
        };
        let search_interrupt = search_reader.interrupt_handle();
        Ok(Session {
            id,
            profile,
            manager,
            caps,
            location,
            ephemeral_dir,
            router: tokio::sync::Mutex::new(router),
            scan_options: Mutex::new(scan_options),
            scan_notes: Mutex::new(Vec::new()),
            live: tokio::sync::Mutex::new(None),
            live_on: AtomicBool::new(false),
            live_last: Mutex::new(LiveStatus::default()),
            live_applied: Mutex::new((0, 0)),
            open_key: Mutex::new(None),
            reader: Mutex::new(reader),
            anchor_reader: Mutex::new(anchor_reader),
            anchors: Mutex::new(None),
            anchors_building: AtomicBool::new(false),
            writer: tokio::sync::Mutex::new(Some(writer)),
            scan: tokio::sync::Mutex::new(None),
            last_progress: Mutex::new(None),
            index_mode,
            index_mode_auto,
            job: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            pager: tokio::sync::Mutex::new(None),
            console: tokio::sync::Mutex::new(None),
            index_stale: AtomicBool::new(false),
            fold_cache: Mutex::new(HashMap::new()),
            fold_rules: Mutex::new(HashMap::new()),
            search_reader: Mutex::new(search_reader),
            search_interrupt,
            search_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            fts_cancel: Arc::new(AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            delete: tokio::sync::Mutex::new(None),
        })
    }

    /// Cancel the search in flight and hand out the flag for a new one.
    pub fn new_search(&self) -> Arc<AtomicBool> {
        self.cancel_search();
        let fresh = Arc::new(AtomicBool::new(false));
        if let Ok(mut slot) = self.search_cancel.lock() {
            *slot = Arc::clone(&fresh);
        }
        fresh
    }

    /// The flag of the search in flight (for its count or verification).
    pub fn current_search(&self) -> Arc<AtomicBool> {
        self.search_cancel
            .lock()
            .map(|s| Arc::clone(&s))
            .unwrap_or_else(|_| Arc::new(AtomicBool::new(true)))
    }

    /// Raise the flag of the search in flight and abort its statement.
    pub fn cancel_search(&self) {
        if let Ok(slot) = self.search_cancel.lock() {
            slot.store(true, Ordering::Relaxed);
        }
        self.search_interrupt.interrupt();
    }

    /// Borrow the writable index for a blocking operation; `None` while a
    /// scan (or another such operation) owns it.
    pub async fn take_writer(&self) -> Option<Index> {
        self.writer.lock().await.take()
    }

    /// Hand the writer back, first applying the write-through outcomes
    /// that queued up while it was away.
    pub async fn return_writer(&self, mut index: Index) {
        let pending: Vec<Outcome> = self.pending.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
        if !pending.is_empty() {
            let n = pending.len();
            index = tokio::task::spawn_blocking(move || {
                for o in &pending {
                    if let Err(e) = mutate::write_through(&mut index, o) {
                        tracing::warn!("queued write-through failed: {e}");
                    }
                }
                index
            })
            .await
            .unwrap_or_else(|e| panic!("writer task failed: {e}"));
            tracing::info!(id = self.id, n, "applied queued write-through");
            self.invalidate_anchors();
        }
        *self.writer.lock().await = Some(index);
    }

    /// Reopen the writer after the task that owned it failed.
    pub async fn reopen_writer(&self) {
        let location = self.location.clone();
        match tokio::task::spawn_blocking(move || Index::open(location, &IndexOptions::default())).await {
            Ok(Ok(index)) => self.return_writer(index).await,
            Ok(Err(e)) => tracing::error!("could not reopen index: {e}"),
            Err(e) => tracing::error!("reopen task failed: {e}"),
        }
    }

    /// Bring the index in line with a mutation now, or queue it when a
    /// scan owns the writer. Returns the change and whether it was queued.
    pub async fn write_through(&self, outcome: Outcome) -> Result<(IndexChange, bool), String> {
        self.write_through_many(vec![outcome]).await
    }

    /// [`Session::write_through`] for a batch (keyspace notifications),
    /// taking the writer once.
    pub async fn write_through_many(&self, outcomes: Vec<Outcome>) -> Result<(IndexChange, bool), String> {
        if outcomes.is_empty() {
            return Ok((IndexChange::default(), false));
        }
        let Some(mut index) = self.take_writer().await else {
            self.pending.lock().map_err(|_| "pending poisoned")?.extend(outcomes);
            return Ok((IndexChange::default(), true));
        };
        let (index, result) = tokio::task::spawn_blocking(move || {
            let mut total = IndexChange::default();
            let mut failed = None;
            for o in &outcomes {
                match mutate::write_through(&mut index, o) {
                    Ok(c) => {
                        total.added += c.added;
                        total.removed += c.removed;
                    }
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            (index, failed.map_or(Ok(total), Err))
        })
        .await
        .map_err(|e| e.to_string())?;
        self.return_writer(index).await;
        let change = result.map_err(|e| e.to_string())?;
        if !change.is_empty() {
            self.invalidate_anchors();
        }
        Ok((change, false))
    }

    /// Row counts moved behind the UI's back: drop anchors and fold caches.
    pub fn invalidate_views(&self) {
        self.invalidate_anchors();
    }

    pub fn index_info(&self) -> Result<IndexInfo, String> {
        let mut reader = self.reader.lock().map_err(|_| "reader poisoned")?;
        reader.reload_meta().map_err(|e| e.to_string())?;
        let state = reader.scan_state().map_err(|e| e.to_string())?;
        let full_at = reader.coverage(b"").map_err(|e| e.to_string())?.map(|c| c.at);
        let subtrees = reader
            .subtree_scans()
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|s| s.finished_at.is_some())
            .count();
        Ok(IndexInfo {
            keys: reader.key_count(),
            status: status_name(state.status),
            generation: state.generation,
            scanned_at: state.finished_at,
            full_at,
            subtrees,
            ephemeral: self.ephemeral_dir.is_some(),
            path: match &self.location {
                Location::File(p) => p.display().to_string(),
                Location::Memory => "memory".into(),
            },
        })
    }

    /// The connection as the header shows it; `DBSIZE` is re-read (summed
    /// over the primaries) so the number is current after a delete or flush.
    pub async fn dto(&self) -> Result<ConnectionDto, String> {
        let scanning = self.scan.lock().await.is_some();
        let (dbsize, nodes, cluster, note) = {
            let mut router = self.router.lock().await;
            let dbsize = router.dbsize().await.or(self.caps.dbsize);
            let nodes = scan::known_nodes(&self.caps, router.topology(), router.seed())
                .into_iter()
                .map(|(addr, role, online)| NodeDto {
                    addr: addr.to_string(),
                    role: role.as_str(),
                    online,
                })
                .collect();
            let cluster = router.topology().map(|t| ClusterDto {
                shards: t.shards.len(),
                nodes: t.node_count(),
                unassigned_slots: t.unassigned_slots(),
            });
            (dbsize, nodes, cluster, router.note().map(str::to_string))
        };
        let notify = self.notify_setting().await.ok().map(|s| NotifySettingDto {
            enabled: s.keyevents_enabled(),
            value: s.value,
        });
        let (profile, target) = *self.scan_options.lock().map_err(|_| "options poisoned")?;
        Ok(ConnectionDto {
            id: self.id,
            label: self.profile.name.clone(),
            tag: self.profile.tag.as_str(),
            server: self.caps.server.to_string(),
            version: self.caps.version.clone(),
            mode: self.caps.mode.clone(),
            role: self.caps.role.clone(),
            protocol: self.caps.protocol,
            db: self.caps.db,
            dbsize,
            used_memory: self.caps.used_memory,
            denied: self.caps.denied.list(),
            index: self.index_info()?,
            index_mode: self.index_mode.as_str(),
            index_mode_auto: self.index_mode_auto,
            scanning,
            scan_queue: self.queue.lock().map(|q| q.len()).unwrap_or(0),
            index_stale: self.index_stale.load(Ordering::Relaxed),
            nodes,
            cluster,
            note,
            scan_profile: profile.as_str(),
            scan_target: target.as_str(),
            scan_notes: self.scan_notes.lock().map(|n| n.clone()).unwrap_or_default(),
            live: self.live_status(),
            notify,
        })
    }

    /// A connection without an index has no tree, list, search or
    /// pattern delete to offer.
    pub fn require_index(&self) -> Result<(), String> {
        if self.index_mode == IndexMode::None {
            return Err("this connection keeps no index; choose the on-demand or the full index to use this".into());
        }
        Ok(())
    }

    /// The scan a connection starts with: everything for the full index,
    /// a sample for an on-demand index that is still empty, nothing else.
    pub async fn start_default(self: &Arc<Self>, app: AppHandle) -> Result<(), String> {
        match self.index_mode {
            IndexMode::Full => self.start_job(app, ScanJob::Full).await,
            IndexMode::OnDemand => {
                let empty = {
                    let mut reader = self.reader.lock().map_err(|_| "reader poisoned")?;
                    reader.reload_meta().map_err(|e| e.to_string())?;
                    reader.key_count() == 0
                };
                if empty {
                    self.start_job(app, ScanJob::Sample).await
                } else {
                    Ok(())
                }
            }
            IndexMode::None => Ok(()),
        }
    }

    /// How the index stands under `prefix`: when it was last scanned to
    /// the end, whether a scan that covers it runs or waits, and whether
    /// its counts are exact.
    pub fn prefix_state(&self, reader: &Index, prefix: &[u8]) -> Result<PrefixState, String> {
        let scanned_at = reader.coverage(prefix).map_err(|e| e.to_string())?.map(|c| c.at);
        let job = self.job.lock().map_err(|_| "job poisoned")?.clone();
        // a scan over the prefix makes it fresh; one beneath it only moves its counts
        let (running, moving) = match &job {
            None => (false, false),
            Some(ScanJob::Full) | Some(ScanJob::Sample) => (job == Some(ScanJob::Full), true),
            Some(ScanJob::Subtree(p)) => (prefix.starts_with(p), prefix.starts_with(p) || p.starts_with(prefix)),
        };
        let queued = !running
            && self
                .queue
                .lock()
                .map_err(|_| "queue poisoned")?
                .iter()
                .any(|q| prefix.starts_with(q));
        Ok(PrefixState {
            scanned_at,
            scan: if running {
                "running"
            } else if queued {
                "queued"
            } else {
                "idle"
            },
            exact: scanned_at.is_some() && !moving,
        })
    }

    /// Scan the subtree under `prefix` now, or after the scans that are
    /// running and waiting. Returns `started`, `queued`, or `covered` when
    /// a running or waiting scan already takes care of it.
    pub async fn request_subtree(self: &Arc<Self>, app: AppHandle, prefix: Vec<u8>) -> Result<&'static str, String> {
        if prefix.is_empty() {
            return Err("a subtree scan needs a prefix".into());
        }
        let covered = match &*self.job.lock().map_err(|_| "job poisoned")? {
            Some(ScanJob::Full) => true,
            Some(ScanJob::Subtree(p)) => prefix.starts_with(p),
            _ => false,
        };
        if covered {
            return Ok("covered");
        }
        if self.is_scanning().await {
            let mut queue = self.queue.lock().map_err(|_| "queue poisoned")?;
            if queue.iter().any(|q| prefix.starts_with(q)) {
                return Ok("covered");
            }
            // a scan of this prefix makes the ones beneath it redundant
            queue.retain(|q| !q.starts_with(&prefix));
            queue.push_back(prefix);
            return Ok("queued");
        }
        self.start_job(app, ScanJob::Subtree(prefix)).await?;
        Ok("started")
    }

    /// Stop the running scan (its cursors stay, so it can be resumed) and
    /// forget the subtree scans that were waiting.
    pub async fn cancel_scans(&self) {
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
        if let Some(h) = self.scan.lock().await.as_ref() {
            h.stop();
        }
    }

    /// Start (or resume) a scan on its own connections and forward progress
    /// to the webview as `scan:progress`, at most four times a second. A
    /// scan that is already running stays; when it ends, the next waiting
    /// subtree scan starts.
    pub async fn start_job(self: &Arc<Self>, app: AppHandle, job: ScanJob) -> Result<(), String> {
        self.require_index()?;
        let mut scan_slot = self.scan.lock().await;
        if scan_slot.is_some() {
            return Ok(());
        }
        let (profile, target) = *self.scan_options.lock().map_err(|_| "options poisoned")?;
        let plan = {
            let router = self.router.lock().await;
            scan::targets(&self.manager, &self.caps, router.topology(), target, profile).await
        };
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                let msg = e.to_string();
                if let Ok(mut n) = self.scan_notes.lock() {
                    *n = vec![msg.clone()];
                }
                return Err(msg);
            }
        };
        if let Ok(mut n) = self.scan_notes.lock() {
            *n = plan.notes.clone();
        }
        tracing::info!(id = self.id, job = job.name(), nodes = %scan::describe(&plan.nodes), profile = profile.as_str(), target = target.as_str(), notes = ?plan.notes, "scan targets");
        let index = self.writer.lock().await.take().ok_or("index is busy")?;
        let config = ScanConfig {
            profile,
            subtree: match &job {
                ScanJob::Subtree(p) => Some(p.clone()),
                _ => None,
            },
            sample: (job == ScanJob::Sample).then_some(SAMPLE_KEYS),
            ..ScanConfig::default()
        };
        let handle = scan::start_nodes(plan.nodes, index, config);
        let mut progress = handle.progress();
        *scan_slot = Some(handle);
        if let Ok(mut j) = self.job.lock() {
            *j = Some(job);
        }
        drop(scan_slot);

        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut last_emit = Instant::now() - Duration::from_secs(1);
            loop {
                if progress.changed().await.is_err() {
                    break;
                }
                let mut p = progress.borrow().clone();
                let terminal = matches!(p.phase, ScanPhase::Complete | ScanPhase::Stopped | ScanPhase::Failed);
                if !terminal {
                    let since = last_emit.elapsed();
                    if since < Duration::from_millis(250) {
                        tokio::time::sleep(Duration::from_millis(250) - since).await;
                        p = progress.borrow().clone();
                    }
                }
                session.emit_progress(&app, &p);
                last_emit = Instant::now();
                if matches!(p.phase, ScanPhase::Complete | ScanPhase::Stopped | ScanPhase::Failed) {
                    break;
                }
            }
            // Take the index back (or reopen it after a failure).
            let handle = session.scan.lock().await.take();
            let mut final_progress = None;
            if let Some(h) = handle {
                match h.wait().await {
                    Ok(outcome) => {
                        session.return_writer(outcome.index).await;
                        final_progress = Some(outcome.progress);
                    }
                    Err(e) => {
                        tracing::warn!("scan ended with error: {e}");
                        session.reopen_writer().await;
                        let mut p = progress.borrow().clone();
                        p.phase = ScanPhase::Failed;
                        p.error = Some(e.to_string());
                        final_progress = Some(p);
                    }
                }
            }
            session.invalidate_anchors();
            if let Some(p) = &final_progress
                && p.phase == ScanPhase::Complete
                && session.job_name() == "full"
            {
                // everything was read again: whatever the console did is in the index now
                session.index_stale.store(false, Ordering::Relaxed);
            }
            if let Some(p) = final_progress {
                tracing::info!(id = session.id, job = session.job_name(), phase = ?p.phase, scanned = p.scanned, new = p.indexed_new, pruned = p.pruned, calls = p.calls, took = ?p.elapsed, "scan ended");
                session.emit_progress(&app, &p);
            }
            if let Ok(mut j) = session.job.lock() {
                *j = None;
            }
            let next = session.queue.lock().ok().and_then(|mut q| q.pop_front());
            if let Some(prefix) = next {
                start_next(session, app, prefix);
            }
        });
        Ok(())
    }

    fn job_name(&self) -> &'static str {
        self.job.lock().ok().and_then(|j| j.as_ref().map(ScanJob::name)).unwrap_or("full")
    }

    fn emit_progress(&self, app: &AppHandle, p: &ScanProgress) {
        let index_keys = self
            .reader
            .lock()
            .ok()
            .and_then(|mut r| r.reload_meta().ok().map(|_| r.key_count()))
            .unwrap_or(0);
        let dto = ProgressDto {
            session: self.id,
            job: self.job_name(),
            subtree: p.subtree.as_deref().map(KeyDto::new),
            fraction: p.fraction(),
            estimate: p.is_estimate(),
            scan_queue: self.queue.lock().map(|q| q.len()).unwrap_or(0),
            phase: phase_name(p.phase),
            generation: p.generation,
            resumed: p.resumed,
            partial: p.partial,
            dbsize: p.dbsize,
            scanned: p.scanned,
            indexed_new: p.indexed_new,
            calls: p.calls,
            count: p.count,
            last_rtt_ms: p.last_rtt.as_secs_f64() * 1000.0,
            elapsed_ms: p.elapsed.as_secs_f64() * 1000.0,
            queued: p.queued,
            pruned: p.pruned,
            index_keys,
            error: p.error.clone(),
            nodes: p
                .nodes
                .iter()
                .map(|n| NodeProgressDto {
                    label: n.label.clone(),
                    role: n.role.as_str(),
                    dbsize: n.dbsize,
                    scanned: n.scanned,
                    calls: n.calls,
                    count: n.count,
                    done: n.done,
                    backoff: n.backoff,
                    error: n.error.clone(),
                })
                .collect(),
            backoff: p.backoff,
            scan_notes: self.scan_notes.lock().map(|n| n.clone()).unwrap_or_default(),
        };
        if let Ok(mut last) = self.last_progress.lock() {
            *last = Some(dto.clone());
        }
        let _ = app.emit("scan:progress", &dto);
    }

    pub async fn pause(&self) {
        if let Some(h) = self.scan.lock().await.as_ref() {
            h.pause();
        }
    }

    pub async fn resume(&self) {
        if let Some(h) = self.scan.lock().await.as_ref() {
            h.resume();
        }
    }

    pub async fn is_scanning(&self) -> bool {
        self.scan.lock().await.is_some()
    }

    fn invalidate_anchors(&self) {
        if let Ok(mut a) = self.anchors.lock() {
            *a = None;
        }
        if let Ok(mut c) = self.fold_cache.lock() {
            c.clear();
        }
    }

    /// Anchors for row addressing. Built synchronously when there are none,
    /// otherwise refreshed in the background once the key count has moved by
    /// more than one percent; the list keeps using the old ones meanwhile.
    pub fn anchors(self: &Arc<Self>, key_count: i64) -> Result<Arc<Anchors>, String> {
        let current = self.anchors.lock().map_err(|_| "anchors poisoned")?.clone();
        match current {
            Some(a) => {
                let drift = (a.total - key_count).abs();
                if drift * 100 > key_count.max(1) && !self.anchors_building.swap(true, Ordering::AcqRel) {
                    let session = Arc::clone(self);
                    tokio::task::spawn_blocking(move || {
                        let built = session
                            .anchor_reader
                            .lock()
                            .ok()
                            .and_then(|r| r.build_anchors(Anchors::DEFAULT_STEP).ok());
                        if let (Some(b), Ok(mut slot)) = (built, session.anchors.lock()) {
                            *slot = Some(Arc::new(b));
                        }
                        session.anchors_building.store(false, Ordering::Release);
                    });
                }
                Ok(a)
            }
            None => {
                let built = self
                    .reader
                    .lock()
                    .map_err(|_| "reader poisoned")?
                    .build_anchors(Anchors::DEFAULT_STEP)
                    .map_err(|e| e.to_string())?;
                let a = Arc::new(built);
                *self.anchors.lock().map_err(|_| "anchors poisoned")? = Some(Arc::clone(&a));
                Ok(a)
            }
        }
    }

    /// Verify-on-open found the key gone: drop it from the index right away
    /// when no scan owns the writer; otherwise the next generation prunes it.
    pub async fn forget_key(&self, key: Vec<u8>) {
        let Some(mut index) = self.take_writer().await else { return };
        let (index, removed) = tokio::task::spawn_blocking(move || {
            let removed = index.remove(&[key]).unwrap_or(0);
            (index, removed)
        })
        .await
        .unwrap_or_else(|e| panic!("writer task failed: {e}"));
        self.return_writer(index).await;
        if removed > 0 {
            self.invalidate_anchors();
        }
    }

    /// Stop a running scan (cursor stays persisted) and delete an ephemeral index.
    pub async fn close(&self) {
        self.cancel_search();
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
        self.pager.lock().await.take();
        self.console.lock().await.take();
        self.fts_cancel.store(true, Ordering::Relaxed);
        self.stop_live().await;
        if let Some(h) = self.scan.lock().await.take() {
            h.stop();
            let _ = h.wait().await;
        }
        if let Some(h) = self.delete.lock().await.take() {
            h.cancel();
            let _ = h.wait().await;
        }
        self.writer.lock().await.take();
        if let Some(dir) = &self.ephemeral_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Why a connect failed, for the webview. A host key that is not on
/// record yet is a question to the user, not a failure: the text starts
/// with `ssh-host-key-unknown` and carries the key as JSON, which the
/// webview shows and hands to `ssh_trust` when the user accepts it.
pub fn connect_error(e: keyfold_core::Error) -> String {
    match &e {
        keyfold_core::Error::HostKey(k) if !k.changed => format!(
            "ssh-host-key-unknown {}",
            serde_json::json!({ "host": k.host, "port": k.port, "fingerprint": k.fingerprint, "openssh": k.openssh })
        ),
        _ => e.to_string(),
    }
}

/// The next waiting subtree scan, started from the task of the one that
/// just ended.
fn start_next(session: Arc<Session>, app: AppHandle, prefix: Vec<u8>) {
    tokio::spawn(async move {
        if let Err(e) = session.start_job(app, ScanJob::Subtree(prefix)).await {
            tracing::warn!(id = session.id, "queued subtree scan did not start: {e}");
        }
    });
}
