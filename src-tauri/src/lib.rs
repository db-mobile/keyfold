//! Tauri shell. Commands are the backend surface from CONCEPT section 10,
//! always windowed; events carry scan progress. The webview never holds the
//! keyspace.

mod browse;
mod console;
mod live;
mod memory;
mod mutate;
mod profiles;
mod search;
mod secrets;
mod session;
#[cfg(target_os = "linux")]
mod system_theme;
mod values;
#[cfg(target_os = "linux")]
mod window_size;
mod wire;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use keyfold_core::IndexMode;
use keyfold_core::fold::{self, FoldClass, FoldThresholds, Override};
use keyfold_core::index::{ChildFilter, MAX_WINDOW};
use keyfold_core::segment::SegClass;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tokio::sync::RwLock;

use session::{ConnectionDto, PrefixState, ScanJob, Session};
use wire::{KeyArg, KeyDto, bytes_from, display_and_raw};

#[derive(Default)]
pub struct Sessions {
    next: AtomicU32,
    map: RwLock<HashMap<u32, Arc<Session>>>,
}

impl Sessions {
    pub fn new_id(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub async fn get(&self, id: u32) -> Result<Arc<Session>, String> {
        self.map.read().await.get(&id).cloned().ok_or_else(|| format!("no connection {id}"))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RowDto {
    name: String,
    /// Exact bytes as hex when the name is not UTF-8.
    raw: Option<String>,
    key_type: Option<String>,
    ttl: Option<i64>,
    mem: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListWindowDto {
    total: i64,
    exact: bool,
    rows: Vec<RowDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClassesDto {
    uuid: i64,
    ulid: i64,
    hex: i64,
    numeric: i64,
    date: i64,
    opaque: i64,
    email: i64,
    literal: i64,
}

impl From<keyfold_core::index::ClassCounts> for ClassesDto {
    fn from(c: keyfold_core::index::ClassCounts) -> Self {
        ClassesDto {
            uuid: c.uuid,
            ulid: c.ulid,
            hex: c.hex,
            numeric: c.numeric,
            date: c.date,
            opaque: c.opaque,
            email: c.email,
            literal: c.literal,
        }
    }
}

/// One tree row. Byte strings cross the IPC as text; when a segment or
/// prefix is not valid UTF-8 the text is escaped for display and
/// `raw`/`prefix_raw` carry the exact bytes as hex, which the webview hands
/// back unchanged (see `wire`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeDto {
    segment: String,
    raw: Option<String>,
    /// Prefix to request this node's children (parent + segment + delimiter).
    prefix: String,
    prefix_raw: Option<String>,
    /// The key this node stands for (parent + segment), when it is one.
    key: Option<KeyDto>,
    /// When a completed scan last saw every key under this node (unix
    /// seconds); `None` when the index knows the subtree only in part.
    scanned_at: Option<i64>,
    /// `idle`, `queued` or `running`: a scan that will make it fresh.
    scan: &'static str,
    leaf: bool,
    count: i64,
    seg_num: Option<i64>,
    children: i64,
    classes: ClassesDto,
}

/// The last row of the previous window, for keyset paging.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TreeAfter {
    segment: String,
    raw: Option<String>,
    count: i64,
    seg_num: Option<i64>,
}

/// Narrows a child window; bounds are text with optional exact hex bytes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChildFilterDto {
    seg_num_min: Option<i64>,
    seg_num_max: Option<i64>,
    seg_min: Option<String>,
    seg_min_raw: Option<String>,
    seg_max: Option<String>,
    seg_max_raw: Option<String>,
    class: Option<String>,
    exclude_classes: Option<Vec<String>>,
}

fn class_from(s: &str) -> Result<SegClass, String> {
    Ok(match s {
        "uuid" => SegClass::Uuid,
        "ulid" => SegClass::Ulid,
        "hex" => SegClass::Hex,
        "numeric" => SegClass::Numeric,
        "date" => SegClass::Date,
        "opaque" => SegClass::Opaque,
        "email" => SegClass::Email,
        "literal" => SegClass::Literal,
        other => return Err(format!("unknown class {other:?}")),
    })
}

fn filter_from(dto: Option<ChildFilterDto>) -> Result<ChildFilter, String> {
    let Some(d) = dto else { return Ok(ChildFilter::default()) };
    let seg_num = match (d.seg_num_min, d.seg_num_max) {
        (Some(lo), Some(hi)) => Some((lo, hi)),
        (Some(lo), None) => Some((lo, i64::MAX)),
        (None, Some(hi)) => Some((i64::MIN, hi)),
        (None, None) => None,
    };
    let segment = match d.seg_min {
        Some(lo) => {
            let lo = bytes_from(&lo, d.seg_min_raw.as_deref())?;
            let hi = match d.seg_max {
                Some(hi) => Some(bytes_from(&hi, d.seg_max_raw.as_deref())?),
                None => None,
            };
            Some((lo, hi))
        }
        None => None,
    };
    let class = d.class.as_deref().map(class_from).transpose()?;
    let exclude_classes = d
        .exclude_classes
        .unwrap_or_default()
        .iter()
        .map(|c| class_from(c))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChildFilter {
        seg_num,
        segment,
        class,
        exclude_classes,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BucketDto {
    label: String,
    seg_num_min: Option<i64>,
    seg_num_max: Option<i64>,
    seg_min: Option<String>,
    seg_min_raw: Option<String>,
    seg_max: Option<String>,
    seg_max_raw: Option<String>,
    count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FoldEntryDto {
    segment: String,
    raw: Option<String>,
    parents: i64,
    keys: i64,
    leaves: i64,
    /// `parents / members`, 0 to 1.
    coverage: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FoldDto {
    class: String,
    label: String,
    members: i64,
    share: f64,
    range: Option<[i64; 2]>,
    samples: Vec<String>,
    summary: Vec<FoldEntryDto>,
    summary_distinct: i64,
    members_with_children: i64,
    buckets: Vec<BucketDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FoldInfoDto {
    distinct: i64,
    evidence: String,
    folded: bool,
    folds: Vec<FoldDto>,
    /// Children that belong to no fold, as ordinary siblings (at most 200).
    literals: Vec<NodeDto>,
    computed_ms: f64,
    cached: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TreeWindowDto {
    nodes: Vec<NodeDto>,
    exact: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TreeRootDto {
    children: i64,
    classes: ClassesDto,
    delimiter: String,
    keys: i64,
    exact: bool,
    /// When the whole keyspace was last scanned to the end.
    scanned_at: Option<i64>,
}

fn node_dto(session: &Session, reader: &keyfold_core::Index, n: &keyfold_core::Node, delimiter: u8) -> Result<NodeDto, String> {
    let (segment, raw) = display_and_raw(&n.segment);
    let prefix_bytes = n.prefix(delimiter);
    let PrefixState { scanned_at, scan, .. } = session.prefix_state(reader, &prefix_bytes)?;
    let (prefix, prefix_raw) = display_and_raw(&prefix_bytes);
    let key = n.leaf.then(|| KeyDto::new(&[&n.parent[..], &n.segment[..]].concat()));
    Ok(NodeDto {
        segment,
        raw,
        prefix,
        prefix_raw,
        key,
        scanned_at,
        scan,
        leaf: n.leaf,
        count: n.count,
        seg_num: n.seg_num,
        children: n.children,
        classes: n.classes.into(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetaDto {
    exists: bool,
    key_type: Option<String>,
    ttl: Option<i64>,
    mem: Option<i64>,
}

/// Connect, probe, read the cluster topology and start what the index
/// mode starts with. `profile` and `target` preset the scan options
/// (`fast|normal|polite`, `auto|primary|replica`); `index_mode` is `full`,
/// `onDemand` or `none`, and decided by the database's size when absent.
#[tauri::command]
async fn connect(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    url: String,
    tag: String,
    profile: Option<String>,
    target: Option<String>,
    index_mode: Option<String>,
) -> Result<ConnectionDto, String> {
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let id = sessions.next.fetch_add(1, Ordering::Relaxed) + 1;
    let t = std::time::Instant::now();
    let index_mode = mode_from(index_mode.as_deref())?;
    let session = Arc::new(Session::open(id, &url, &tag, index_mode, cache_dir).await?);
    {
        let mut opts = session.scan_options.lock().map_err(|_| "options poisoned")?;
        if let Some(p) = profile.as_deref().filter(|p| !p.is_empty()) {
            opts.0 = keyfold_core::ScanProfile::parse(p).ok_or_else(|| format!("unknown scan profile {p:?}"))?;
        }
        if let Some(tg) = target.as_deref().filter(|t| !t.is_empty()) {
            opts.1 = keyfold_core::ScanTarget::parse(tg).ok_or_else(|| format!("unknown scan target {tg:?}"))?;
        }
    }
    let dto = adopt(&app, &sessions, session).await?;
    tracing::debug!(id, took = ?t.elapsed(), "connect by URL");
    Ok(dto)
}

/// `full`, `onDemand`, `none`; nothing or an empty text means "by size".
fn mode_from(text: Option<&str>) -> Result<Option<IndexMode>, String> {
    match text.filter(|m| !m.is_empty()) {
        Some(m) => Ok(Some(IndexMode::parse(m).ok_or_else(|| format!("unknown index mode {m:?}"))?)),
        None => Ok(None),
    }
}

/// Register an opened session and start what its index mode starts with:
/// the full scan, the sample of an on-demand index, or nothing.
pub(crate) async fn adopt(app: &AppHandle, sessions: &Sessions, session: Arc<Session>) -> Result<ConnectionDto, String> {
    let id = session.id;
    sessions.map.write().await.insert(id, Arc::clone(&session));
    if let Err(e) = session.start_default(app.clone()).await {
        // the connection stays usable; the header shows why no scan runs
        tracing::warn!(id, "scan did not start: {e}");
    }
    let dto = session.dto().await?;
    tracing::info!(id, index_mode = dto.index_mode, label = %session.profile.name, server = %session.caps.server, version = %session.caps.version, mode = %session.caps.mode, dbsize = ?dto.dbsize, cluster = ?dto.cluster, "connected");
    Ok(dto)
}

/// The connection header, with a fresh `DBSIZE`.
#[tauri::command]
async fn connection_info(sessions: State<'_, Sessions>, id: u32) -> Result<ConnectionDto, String> {
    sessions.get(id).await?.dto().await
}

#[tauri::command]
async fn disconnect(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    let session = sessions
        .map
        .write()
        .await
        .remove(&id)
        .ok_or_else(|| format!("no connection {id}"))?;
    session.close().await;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseDto {
    db: i64,
    keys: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DatabasesDto {
    current: i64,
    /// Databases `0..count` can be selected.
    count: u32,
    /// `count` comes from the server's configuration, not the default.
    configured: bool,
    /// The non-empty databases with their key counts.
    used: Vec<DatabaseDto>,
}

/// The logical databases to switch between, for the header's selector.
#[tauri::command]
async fn databases(sessions: State<'_, Sessions>, id: u32) -> Result<DatabasesDto, String> {
    let session = sessions.get(id).await?;
    let dbs = session.router.lock().await.databases().await;
    Ok(DatabasesDto {
        current: session.profile.db,
        count: dbs.count.max(session.profile.db as u32 + 1),
        configured: dbs.configured,
        used: dbs.keys.into_iter().map(|(db, keys)| DatabaseDto { db, keys }).collect(),
    })
}

/// Switch to another logical database of the same server: a new session
/// (new id, own index file) with the tag, credentials and scan options of
/// the old one, which is closed once the new one is open. When the server
/// refuses the database the old session stays as it was.
#[tauri::command]
async fn switch_db(app: AppHandle, sessions: State<'_, Sessions>, id: u32, db: i64) -> Result<ConnectionDto, String> {
    if db < 0 {
        return Err(format!("no database {db}"));
    }
    let old = sessions.get(id).await?;
    if old.profile.db == db {
        return old.dto().await;
    }
    let t = std::time::Instant::now();
    let dto = replace_session(&app, &sessions, old.clone(), old.profile.with_db(db), false).await?;
    tracing::info!(from = id, id = dto.id, db, dbsize = ?dto.dbsize, took = ?t.elapsed(), "switched database");
    Ok(dto)
}

/// Open a session for `profile` in place of `old`, with the old one's
/// scan options. `close_first` when both use the same index files (a
/// change of the index mode): the old session then lets go of them before
/// the new one opens, and a failure leaves no connection.
async fn replace_session(
    app: &AppHandle,
    sessions: &Sessions,
    old: Arc<Session>,
    profile: keyfold_core::Profile,
    close_first: bool,
) -> Result<ConnectionDto, String> {
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let new_id = sessions.next.fetch_add(1, Ordering::Relaxed) + 1;
    let options = *old.scan_options.lock().map_err(|_| "options poisoned")?;
    if close_first {
        old.close().await;
    }
    let tunnel = old.manager.tunnel().cloned();
    let session = match Session::open_profile(new_id, profile, cache_dir, tunnel).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            if close_first {
                sessions.map.write().await.remove(&old.id);
                return Err(format!("the connection was closed and could not be opened again: {e}"));
            }
            return Err(e);
        }
    };
    *session.scan_options.lock().map_err(|_| "options poisoned")? = options;
    {
        let mut map = sessions.map.write().await;
        map.remove(&old.id);
        map.insert(new_id, Arc::clone(&session));
    }
    if !close_first {
        old.close().await;
    }
    if let Err(e) = session.start_default(app.clone()).await {
        tracing::warn!(id = new_id, "scan did not start: {e}");
    }
    session.dto().await
}

/// Change how much the connection indexes (`full`, `onDemand`, `none`, or
/// nothing for "by size"): a new session on the same server and database,
/// like a database switch. An index on disk is kept; the choice is written
/// to the stored profile when the connection has one.
#[tauri::command]
async fn set_index_mode(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    store: State<'_, Arc<profiles::ProfileStore>>,
    id: u32,
    mode: Option<String>,
) -> Result<ConnectionDto, String> {
    let mode = mode_from(mode.as_deref())?;
    let old = sessions.get(id).await?;
    if old.delete.lock().await.is_some() {
        return Err("a delete is running".into());
    }
    let mut profile = old.profile.clone();
    profile.index_mode = mode;
    let dto = replace_session(&app, &sessions, old.clone(), profile, true).await?;
    profiles::remember_index_mode(&store, &old.profile.id, mode).await;
    tracing::info!(
        from = id,
        id = dto.id,
        index_mode = dto.index_mode,
        auto = dto.index_mode_auto,
        "index mode changed"
    );
    Ok(dto)
}

/// Scan everything: the full index's scan (or its resume), and "scan
/// everything" on demand, which needs a confirmation because the database
/// was judged too large to index without asking.
#[tauri::command]
async fn scan_start(app: AppHandle, sessions: State<'_, Sessions>, id: u32, confirmed: Option<bool>) -> Result<(), String> {
    let session = sessions.get(id).await?;
    if session.index_mode == IndexMode::OnDemand && confirmed != Some(true) {
        return Err("scanning everything into an on-demand index needs a confirmation".into());
    }
    session.start_job(app, ScanJob::Full).await
}

/// Scan the subtree under a prefix (CONCEPT 4 "subtree rescan", CONCEPT 7
/// "on-demand index"): now, or after the scans already running and
/// waiting. The server walks its whole key table for it, so a prod-tagged
/// connection needs a confirmation. Returns `started`, `queued` or
/// `covered` (a running or waiting scan already includes it).
#[tauri::command]
async fn scan_subtree(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    id: u32,
    prefix: KeyArg,
    confirmed: bool,
) -> Result<&'static str, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    if session.profile.tag == keyfold_core::Tag::Prod && !confirmed {
        return Err("this connection is tagged prod: a subtree scan needs a confirmation".into());
    }
    if session.delete.lock().await.is_some() {
        return Err("a delete is running".into());
    }
    let prefix = prefix.bytes()?;
    let shown = display_and_raw(&prefix).0;
    let outcome = session.request_subtree(app, prefix).await?;
    tracing::info!(id, prefix = %shown, outcome, "scan_subtree");
    Ok(outcome)
}

/// Stop the running scan and drop the waiting subtree scans. What was
/// read stays; a stopped scan resumes from its cursors when started again.
#[tauri::command]
async fn scan_cancel(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    sessions.get(id).await?.cancel_scans().await;
    Ok(())
}

#[tauri::command]
async fn scan_pause(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    sessions.get(id).await?.pause().await;
    Ok(())
}

#[tauri::command]
async fn scan_resume(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    sessions.get(id).await?.resume().await;
    Ok(())
}

/// One window of the flat list by row number. Row numbers are exact once a
/// scan is complete; during a scan they drift with the growing index.
#[tauri::command]
async fn list_rows(sessions: State<'_, Sessions>, id: u32, offset: usize, limit: usize) -> Result<ListWindowDto, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let limit = limit.min(MAX_WINDOW);
    let t = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || -> Result<ListWindowDto, String> {
        let (key_count, exact) = {
            let mut reader = session.reader.lock().map_err(|_| "reader poisoned")?;
            reader.reload_meta().map_err(|e| e.to_string())?;
            (reader.key_count(), session.prefix_state(&reader, b"")?.exact)
        };
        let anchors = session.anchors(key_count)?;
        let rows = session
            .reader
            .lock()
            .map_err(|_| "reader poisoned")?
            .rows_at(&anchors, offset, limit)
            .map_err(|e| e.to_string())?;
        Ok(ListWindowDto {
            total: anchors.total.max(key_count),
            exact,
            rows: rows
                .into_iter()
                .map(|r| {
                    let (name, raw) = display_and_raw(&r.name);
                    RowDto {
                        name,
                        raw,
                        key_type: None,
                        ttl: r.ttl,
                        mem: r.mem,
                    }
                })
                .collect(),
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Ok(w) = &result {
        tracing::debug!(id, offset, limit, rows = w.rows.len(), total = w.total, exact = w.exact, took = ?t.elapsed(), "list_rows");
    }
    result
}

/// Counters of the tree root and the delimiter the tree was built with.
#[tauri::command]
async fn tree_root(sessions: State<'_, Sessions>, id: u32) -> Result<TreeRootDto, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    tokio::task::spawn_blocking(move || -> Result<TreeRootDto, String> {
        let mut reader = session.reader.lock().map_err(|_| "reader poisoned")?;
        reader.reload_meta().map_err(|e| e.to_string())?;
        let root = reader.root().map_err(|e| e.to_string())?;
        let state = session.prefix_state(&reader, b"")?;
        Ok(TreeRootDto {
            children: root.children,
            classes: root.classes.into(),
            delimiter: reader.delimiter().map(|d| (d as char).to_string()).unwrap_or_default(),
            keys: reader.key_count(),
            exact: state.exact,
            scanned_at: state.scanned_at,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One window of a node's children. `prefix` is `""` for the root, else
/// the node's path including the trailing delimiter.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn tree_children(
    sessions: State<'_, Sessions>,
    id: u32,
    prefix: String,
    prefix_raw: Option<String>,
    after: Option<TreeAfter>,
    limit: usize,
    sort: String,
    filter: Option<ChildFilterDto>,
) -> Result<TreeWindowDto, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let filter = filter_from(filter)?;
    let sort = match sort.as_str() {
        "name" => keyfold_core::Sort::Name,
        "count" => keyfold_core::Sort::Count,
        "numeric" => keyfold_core::Sort::Numeric,
        other => return Err(format!("unknown sort {other:?}")),
    };
    let prefix = bytes_from(&prefix, prefix_raw.as_deref())?;
    let after_node = match after {
        Some(a) => Some(keyfold_core::Node {
            parent: prefix.clone(),
            segment: bytes_from(&a.segment, a.raw.as_deref())?,
            leaf: false,
            count: a.count,
            seg_num: a.seg_num,
            children: 0,
            classes: Default::default(),
            len_min: None,
            len_max: None,
        }),
        None => None,
    };
    let t = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || -> Result<TreeWindowDto, String> {
        let reader = session.reader.lock().map_err(|_| "reader poisoned")?;
        let delimiter = reader.delimiter().unwrap_or(b':');
        let state = session.prefix_state(&reader, &prefix)?;
        let nodes = reader
            .children_filtered(&prefix, after_node.as_ref(), limit.min(MAX_WINDOW), sort, &filter)
            .map_err(|e| e.to_string())?;
        Ok(TreeWindowDto {
            nodes: nodes
                .iter()
                .map(|n| node_dto(&session, &reader, n, delimiter))
                .collect::<Result<_, _>>()?,
            exact: state.exact,
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Ok(w) = &result {
        tracing::debug!(id, nodes = w.nodes.len(), ?sort, took = ?t.elapsed(), "tree_children");
    }
    result
}

/// Children within the filter's SQL bounds, for the fold filter box.
#[tauri::command]
async fn tree_count(
    sessions: State<'_, Sessions>,
    id: u32,
    prefix: String,
    prefix_raw: Option<String>,
    filter: Option<ChildFilterDto>,
) -> Result<i64, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let prefix = bytes_from(&prefix, prefix_raw.as_deref())?;
    let filter = filter_from(filter)?;
    tokio::task::spawn_blocking(move || -> Result<i64, String> {
        let reader = session.reader.lock().map_err(|_| "reader poisoned")?;
        reader.count_children(&prefix, &filter).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Set or clear a per-session fold rule for a level.
#[tauri::command]
async fn tree_fold_rule(
    sessions: State<'_, Sessions>,
    id: u32,
    prefix: String,
    prefix_raw: Option<String>,
    rule: String,
) -> Result<(), String> {
    let session = sessions.get(id).await?;
    let key = bytes_from(&prefix, prefix_raw.as_deref())?;
    {
        let mut rules = session.fold_rules.lock().map_err(|_| "rules poisoned")?;
        match rule.as_str() {
            "force" => {
                rules.insert(key.clone(), Override::Force);
            }
            "forbid" => {
                rules.insert(key.clone(), Override::Forbid);
            }
            "none" => {
                rules.remove(&key);
            }
            other => return Err(format!("unknown rule {other:?}")),
        }
    }
    session.fold_cache.lock().map_err(|_| "cache poisoned")?.remove(&key);
    Ok(())
}

/// The fold decision for a level and everything shown inside its folds:
/// labels, ranges, samples, the aggregated structure beneath the members,
/// buckets, and the literal siblings. Cached per session until the subtree
/// count drifts by more than 5 %.
#[tauri::command]
async fn tree_fold(
    sessions: State<'_, Sessions>,
    id: u32,
    prefix: String,
    prefix_raw: Option<String>,
) -> Result<serde_json::Value, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let prefix = bytes_from(&prefix, prefix_raw.as_deref())?;
    let key = prefix.clone();
    let rule = session.fold_rules.lock().map_err(|_| "rules poisoned")?.get(&key).copied();
    let t = std::time::Instant::now();
    tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let reader = session.reader.lock().map_err(|_| "reader poisoned")?;
        let delimiter = reader.delimiter().unwrap_or(b':');
        // the level's own counters: a node row, or the root
        let (distinct, classes, count, hex_len) = if prefix.is_empty() {
            let r = reader.root().map_err(|e| e.to_string())?;
            (r.children, r.classes, reader.key_count(), None)
        } else {
            let body = &prefix[..prefix.len() - 1];
            let (parent, segment) = match body.iter().rposition(|&b| b == delimiter) {
                Some(i) => (&body[..=i], &body[i + 1..]),
                None => (&body[..0], body),
            };
            let Some(n) = reader.node(parent, segment).map_err(|e| e.to_string())? else {
                // a level the index has not seen (an unscanned subtree): nothing to fold
                let dto = FoldInfoDto {
                    distinct: 0,
                    evidence: "this level is not in the index yet".into(),
                    folded: false,
                    folds: Vec::new(),
                    literals: Vec::new(),
                    computed_ms: t.elapsed().as_secs_f64() * 1000.0,
                    cached: false,
                };
                return serde_json::to_value(&dto).map_err(|e| e.to_string());
            };
            let hex_len = (n.len_min == n.len_max).then_some(n.len_min).flatten();
            (n.children, n.classes, n.count, hex_len)
        };
        if let Some((base, cached)) = session.fold_cache.lock().map_err(|_| "cache poisoned")?.get(&key)
            && (base - count).abs() * 20 <= count.max(1)
        {
            let mut v = (**cached).clone();
            v["cached"] = serde_json::Value::Bool(true);
            return Ok(v);
        }
        let thresholds = FoldThresholds::default();
        let similarity = if fold::needs_similarity(distinct, &classes, &thresholds) {
            reader.similarity(&prefix, delimiter, 200).map_err(|e| e.to_string())?
        } else {
            None
        };
        let decision = fold::decide(distinct, &classes, similarity, &thresholds, rule);
        let t_decide = t.elapsed();
        let mut phases: Vec<String> = Vec::new();
        let mut folds = Vec::with_capacity(decision.folds.len());
        for f in &decision.folds {
            let class = f.class.seg_class();
            let range = if f.class == FoldClass::Numeric {
                reader.numeric_range(&prefix).map_err(|e| e.to_string())?
            } else {
                None
            };
            let label = f.class.label(hex_len, range);
            let samples = reader
                .sample_children(&prefix, class, 3)
                .map_err(|e| e.to_string())?
                .iter()
                .map(|s| display_and_raw(s).0)
                .collect();
            let t0 = std::time::Instant::now();
            let summary = reader.fold_summary(&prefix, delimiter, class, 30).map_err(|e| e.to_string())?;
            phases.push(format!("summary {:?}", t0.elapsed()));
            let t0 = std::time::Instant::now();
            let members = f.members.max(1) as f64;
            let buckets = match f.class {
                FoldClass::Numeric => match range {
                    Some((lo, hi)) => reader
                        .numeric_buckets(&prefix, lo, hi)
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|b| BucketDto {
                            label: format!("{} … {}", fold::fmt_count(b.lo), fold::fmt_count(b.hi)),
                            seg_num_min: Some(b.lo),
                            seg_num_max: Some(b.hi),
                            seg_min: None,
                            seg_min_raw: None,
                            seg_max: None,
                            seg_max_raw: None,
                            count: b.count,
                        })
                        .collect(),
                    None => Vec::new(),
                },
                FoldClass::Hex | FoldClass::Uuid | FoldClass::Ulid | FoldClass::Opaque => reader
                    .leading_buckets(&prefix)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|b| {
                        let (lo_text, lo_raw) = display_and_raw(&b.first);
                        let mut hi = b.first.clone();
                        let last = hi.len() - 1;
                        hi[last] = hi[last].saturating_add(1);
                        let (hi_text, hi_raw) = display_and_raw(&hi);
                        BucketDto {
                            label: format!("{lo_text}…"),
                            seg_num_min: None,
                            seg_num_max: None,
                            seg_min: Some(lo_text),
                            seg_min_raw: lo_raw,
                            seg_max: Some(hi_text),
                            seg_max_raw: hi_raw,
                            count: b.count,
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            phases.push(format!("buckets {:?}", t0.elapsed()));
            folds.push(FoldDto {
                class: f.class.name().into(),
                label,
                members: f.members,
                share: f.share,
                range: range.map(|(lo, hi)| [lo, hi]),
                samples,
                summary: summary
                    .entries
                    .iter()
                    .map(|e| {
                        let (segment, raw) = display_and_raw(&e.segment);
                        FoldEntryDto {
                            segment,
                            raw,
                            parents: e.parents,
                            keys: e.keys,
                            leaves: e.leaves,
                            coverage: e.parents as f64 / members,
                        }
                    })
                    .collect(),
                summary_distinct: summary.distinct_segments,
                members_with_children: summary.members_with_children,
                buckets,
            });
        }
        let t0 = std::time::Instant::now();
        let literals = if decision.is_folded() && !decision.folds.iter().any(|f| f.class == FoldClass::Star) {
            let exclude: Vec<SegClass> = decision.folds.iter().filter_map(|f| f.class.seg_class()).collect();
            reader
                .children_filtered(
                    &prefix,
                    None,
                    MAX_WINDOW,
                    keyfold_core::Sort::Name,
                    &ChildFilter {
                        exclude_classes: exclude,
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?
                .iter()
                .map(|n| node_dto(&session, &reader, n, delimiter))
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        phases.push(format!("literals {:?}", t0.elapsed()));
        let folded = decision.is_folded();
        let dto = FoldInfoDto {
            distinct,
            evidence: decision.evidence,
            folded,
            folds,
            literals,
            computed_ms: t.elapsed().as_secs_f64() * 1000.0,
            cached: false,
        };
        let value = serde_json::to_value(&dto).map_err(|e| e.to_string())?;
        session
            .fold_cache
            .lock()
            .map_err(|_| "cache poisoned")?
            .insert(key, (count, Arc::new(value.clone())));
        tracing::debug!(id, distinct, folded = dto.folded, folds = dto.folds.len(), decide = ?t_decide, phases = %phases.join(", "), took = ?t.elapsed(), "tree_fold");
        Ok(value)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Pipelined `TYPE`, `TTL`, `MEMORY USAGE` for the rows on screen.
#[tauri::command]
async fn key_meta(sessions: State<'_, Sessions>, id: u32, names: Vec<KeyArg>) -> Result<Vec<MetaDto>, String> {
    let session = sessions.get(id).await?;
    let names: Vec<Vec<u8>> = names.iter().take(MAX_WINDOW).map(KeyArg::bytes).collect::<Result<_, _>>()?;
    let keys: Vec<&[u8]> = names.iter().map(|n| &n[..]).collect();
    let mut router = session.router.lock().await;
    let t = std::time::Instant::now();
    let metas = keyfold_core::meta::fetch_routed(&mut router, &keys)
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(id, keys = keys.len(), took = ?t.elapsed(), "key_meta");
    Ok(metas
        .into_iter()
        .map(|m| MetaDto {
            exists: m.exists,
            key_type: m.key_type,
            ttl: m.ttl,
            mem: m.mem,
        })
        .collect())
}

/// Frontend errors and notes land in the backend log, where a headless
/// smoke test can see them.
#[tauri::command]
fn app_log(level: String, message: String) {
    match level.as_str() {
        "error" => tracing::error!(target: "webview", "{message}"),
        "warn" => tracing::warn!(target: "webview", "{message}"),
        _ => tracing::info!(target: "webview", "{message}"),
    }
}

/// "dark" or "light" from the desktop, or `None` where the webview's own
/// `prefers-color-scheme` is the answer (see `system_theme`).
#[tauri::command]
fn system_color_scheme() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        let scheme = system_theme::current();
        tracing::debug!(?scheme, "system_color_scheme");
        scheme
    }
    #[cfg(not(target_os = "linux"))]
    None
}

#[tauri::command]
fn memory_stats() -> memory::MemoryStats {
    memory::stats()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupOptions {
    /// `KEYFOLD_AUTOCONNECT=<url>` connects on launch (development aid).
    autoconnect: Option<String>,
    tag: String,
    /// `KEYFOLD_AUTOEXPAND=<prefix>` expands that tree level after the
    /// root loads, which exercises folding in a headless run.
    autoexpand: Option<String>,
    /// `KEYFOLD_AUTOSORT=<name|count|numeric>` changes the tree sort once
    /// the automatic expand is through and logs the rows before and after.
    autosort: Option<String>,
    /// `KEYFOLD_AUTOSELECT=<key>[,<key>...]` selects these keys in turn,
    /// two seconds apart, which exercises the value panel headlessly.
    /// `KEYFOLD_AUTOSELECT_HEX=<hex>[,<hex>...]` adds keys by their exact
    /// bytes, for names that are not UTF-8.
    autoselect: Vec<KeyDto>,
    /// `KEYFOLD_AUTOSEARCH=<query>[,<query>...]` runs these searches in
    /// turn, two seconds apart, and logs what came back.
    autosearch: Vec<String>,
    /// `KEYFOLD_AUTOFTS=1` builds the trigram table once the automatic
    /// searches are through, then runs them again.
    autofts: bool,
    /// `KEYFOLD_AUTOMUTATE=<key>` creates, renames, expires and deletes
    /// that key once the scan is complete, which exercises write-through.
    automutate: Option<String>,
    /// `KEYFOLD_AUTODELETE=<query>` runs a pattern delete for that query
    /// once the scan is complete, with its preview logged.
    autodelete: Option<String>,
    /// `KEYFOLD_AUTOLIVE=1` turns live updates on once the scan is
    /// complete (enabling notifications on the server when they are off)
    /// and logs the status and applied batches.
    autolive: bool,
    /// `KEYFOLD_AUTOSCANOPTS=<profile>:<target>` sets the scan options
    /// before the automatic connect scans (for example `normal:replica`).
    autoscanopts: Option<String>,
    /// `KEYFOLD_AUTOCONSOLE=<line>[;;<line>...]` runs these lines in the
    /// console once the scan is complete, answering every confirmation,
    /// and logs the replies.
    autoconsole: Vec<String>,
    /// `KEYFOLD_AUTOMODE=<full|ondemand|none>` connects in that index mode
    /// whatever the size of the database.
    automode: Option<String>,
    /// `KEYFOLD_AUTOSWITCHDB=<db>` switches to that logical database once
    /// the first scan is complete and logs both connections.
    autoswitchdb: Option<i64>,
    /// `KEYFOLD_AUTOPROFILE=<url>` stores a connection for that URL,
    /// connects it, changes its scan options, lists and deletes it again.
    autoprofile: Option<String>,
    /// `KEYFOLD_AUTOPROFILE_SSH=<user@host:port>,<key file>` gives that
    /// stored connection an SSH tunnel and accepts the bastion's host key.
    autoprofile_ssh: Option<String>,
}

#[tauri::command]
fn startup_options() -> StartupOptions {
    StartupOptions {
        autoconnect: std::env::var("KEYFOLD_AUTOCONNECT").ok().filter(|s| !s.is_empty()),
        tag: std::env::var("KEYFOLD_AUTOCONNECT_TAG").unwrap_or_else(|_| "dev".into()),
        autoexpand: std::env::var("KEYFOLD_AUTOEXPAND").ok().filter(|s| !s.is_empty()),
        autosort: std::env::var("KEYFOLD_AUTOSORT").ok().filter(|s| !s.is_empty()),
        autoselect: std::env::var("KEYFOLD_AUTOSELECT")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter(|k| !k.is_empty())
                    .map(|k| KeyDto::new(k.as_bytes()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into_iter()
            .chain(
                std::env::var("KEYFOLD_AUTOSELECT_HEX")
                    .ok()
                    .iter()
                    .flat_map(|s| s.split(','))
                    .filter_map(|h| wire::unhex(h).ok().filter(|b| !b.is_empty()))
                    .map(|b| KeyDto::new(&b)),
            )
            .collect(),
        autosearch: std::env::var("KEYFOLD_AUTOSEARCH")
            .ok()
            .map(|s| s.split(',').filter(|k| !k.is_empty()).map(str::to_string).collect())
            .unwrap_or_default(),
        autofts: std::env::var("KEYFOLD_AUTOFTS").is_ok_and(|v| v == "1"),
        automutate: std::env::var("KEYFOLD_AUTOMUTATE").ok().filter(|s| !s.is_empty()),
        autodelete: std::env::var("KEYFOLD_AUTODELETE").ok().filter(|s| !s.is_empty()),
        autolive: std::env::var("KEYFOLD_AUTOLIVE").is_ok_and(|v| v == "1"),
        autoscanopts: std::env::var("KEYFOLD_AUTOSCANOPTS").ok().filter(|s| !s.is_empty()),
        automode: std::env::var("KEYFOLD_AUTOMODE").ok().filter(|s| !s.is_empty()),
        autoconsole: std::env::var("KEYFOLD_AUTOCONSOLE")
            .ok()
            .map(|s| s.split(";;").map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default(),
        autoswitchdb: std::env::var("KEYFOLD_AUTOSWITCHDB").ok().and_then(|s| s.parse().ok()),
        autoprofile: std::env::var("KEYFOLD_AUTOPROFILE").ok().filter(|s| !s.is_empty()),
        autoprofile_ssh: std::env::var("KEYFOLD_AUTOPROFILE_SSH").ok().filter(|s| !s.is_empty()),
    }
}

/// What the window-state plugin persists. Its size handling does not round
/// trip under GTK (see `window_size`), so Linux leaves the size out.
fn window_state_flags() -> tauri_plugin_window_state::StateFlags {
    let flags = tauri_plugin_window_state::StateFlags::all();
    #[cfg(target_os = "linux")]
    let flags = flags - tauri_plugin_window_state::StateFlags::SIZE;
    flags
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        // restores size, position and maximised state on start, saves on exit;
        // on Linux the size is handled by `window_size` instead
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(window_state_flags())
                .build(),
        )
        .manage(Sessions::default())
        .setup(|app| {
            // stored connections live in the app data dir (CONCEPT 3.4)
            let data_dir = app.path().app_data_dir()?;
            let path = data_dir.join("profiles.json");
            tracing::info!(path = %path.display(), "profile store");
            // what an older version left readable by others is tightened
            // at every start: the data directory with the stored
            // connections and accepted host keys, and the index folder
            {
                use keyfold_core::private::{own_dir, restrict_existing};
                let indexes = app.path().app_cache_dir()?.join("indexes");
                let done = own_dir(&data_dir)
                    .and_then(|()| restrict_existing(&path))
                    .and_then(|()| restrict_existing(&data_dir.join("known_hosts")))
                    .and_then(|()| own_dir(&indexes));
                if let Err(e) = done {
                    tracing::warn!("could not restrict the app's directories to the user: {e}");
                }
            }
            app.manage(Arc::new(profiles::ProfileStore::new(path)));
            #[cfg(target_os = "linux")]
            if let Some(window) = app.get_webview_window("main") {
                window_size::restore(&window);
            }
            #[cfg(target_os = "linux")]
            system_theme::watch(app.handle());
            Ok(())
        })
        .on_window_event(|_window, _event| {
            #[cfg(target_os = "linux")]
            if matches!(_event, tauri::WindowEvent::CloseRequested { .. }) && _window.label() == "main" {
                window_size::save(_window);
            }
        })
        .on_page_load(|webview, payload| {
            tracing::info!(url = %payload.url(), event = ?payload.event(), label = webview.label(), "page load");
        })
        .invoke_handler(tauri::generate_handler![
            connect,
            disconnect,
            databases,
            switch_db,
            set_index_mode,
            scan_start,
            scan_subtree,
            scan_cancel,
            browse::keys_page,
            console::console_run,
            scan_pause,
            scan_resume,
            list_rows,
            key_meta,
            memory_stats,
            system_color_scheme,
            startup_options,
            app_log,
            tree_root,
            tree_children,
            tree_count,
            tree_fold,
            tree_fold_rule,
            values::value_inspect,
            values::value_page,
            values::value_chunk,
            values::value_dump,
            search::search,
            search::search_count,
            search::search_cancel,
            search::search_fts,
            search::search_verify,
            mutate::mutate,
            mutate::delete_preview,
            mutate::delete_start,
            mutate::delete_pause,
            mutate::delete_resume,
            mutate::delete_cancel,
            mutate::flush,
            connection_info,
            live::notify_setting,
            live::notify_enable,
            live::live_set,
            live::live_status,
            live::scan_options,
            profiles::profiles_list,
            profiles::profile_save,
            profiles::profile_delete,
            profiles::profile_clear_index,
            profiles::profile_from_url,
            profiles::connect_profile,
            profiles::ssh_trust
        ])
        .build(tauri::generate_context!())
        .expect("error while building the application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                let sessions = app.state::<Sessions>();
                let map = sessions.map.blocking_write();
                for session in map.values() {
                    let s = Arc::clone(session);
                    tauri::async_runtime::block_on(async move { s.close().await });
                }
            }
        });
}
