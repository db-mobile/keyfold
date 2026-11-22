//! The local keyspace index: SQLite, schema from CONCEPT section 3.1.
//!
//! One `Index` per connection and logical db. It is a rebuildable cache:
//! an outdated schema is dropped, never migrated.
//!
//! Physically the index is one *keys* file (`keys`, `fold_stats`, `meta`)
//! plus N *node shard* files that each hold the `nodes` rows whose path
//! hashes to that shard (shard 0 also holds `root_stats`). SQLite has one
//! writer per file, so the shards let the scan engine upsert tree rows on
//! several threads at once ([`workers`]); listing children fans out to every
//! shard and merges. The serial API in [`ingest`] uses the same phases.

pub mod batch;
mod coverage;
mod fold_queries;
mod ingest;
mod query;
mod search;
pub mod workers;

pub use batch::KeyBatch;
pub use coverage::{Coverage, ScanSlot, SubtreeRun, SubtreeScan};
pub use fold_queries::{Bucket, CharBucket, ChildFilter, FoldEntry, FoldSummary};
pub use ingest::IngestStats;
pub use query::{Anchors, ClassCounts, MAX_WINDOW, Node, RootStats, Row, Sort};
pub use search::SearchPage;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::error::{Error, Result};

/// Bump when the schema changes; an index with another version is rebuilt.
pub const SCHEMA_VERSION: i64 = 3;

const KEYS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS keys (
  name      BLOB PRIMARY KEY,
  type      INTEGER,
  seen_at   INTEGER NOT NULL,
  ttl       INTEGER,
  mem       INTEGER,
  fts_rowid INTEGER
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS fold_stats (
  path        BLOB PRIMARY KEY,
  similarity  REAL,
  computed_at INTEGER,
  base_count  INTEGER,
  summary     BLOB
);

-- one row per scanned subtree (see `coverage`); the full scan's state is in meta
CREATE TABLE IF NOT EXISTS scans (
  prefix         BLOB PRIMARY KEY,
  generation     INTEGER,
  finished_at    INTEGER,
  run_generation INTEGER,
  run_started_at INTEGER,
  run_cursors    TEXT,
  run_scanned    INTEGER
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v);
"#;

const NODES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
  parent    BLOB NOT NULL,
  segment   BLOB NOT NULL,
  leaf      INTEGER NOT NULL,
  count     INTEGER NOT NULL,
  seg_num   INTEGER,
  children  INTEGER NOT NULL DEFAULT 0,
  c_uuid INTEGER DEFAULT 0, c_ulid INTEGER DEFAULT 0, c_hex INTEGER DEFAULT 0,
  c_num INTEGER DEFAULT 0,  c_date INTEGER DEFAULT 0, c_opaque INTEGER DEFAULT 0,
  c_email INTEGER DEFAULT 0, c_literal INTEGER DEFAULT 0,
  len_min INTEGER, len_max INTEGER,
  PRIMARY KEY (parent, segment)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS nodes_num ON nodes(parent, seg_num) WHERE seg_num IS NOT NULL;

-- the root has no row in nodes; its child counters live here (one row, used in shard 0)
CREATE TABLE IF NOT EXISTS root_stats (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  children  INTEGER NOT NULL DEFAULT 0,
  c_uuid INTEGER DEFAULT 0, c_ulid INTEGER DEFAULT 0, c_hex INTEGER DEFAULT 0,
  c_num INTEGER DEFAULT 0,  c_date INTEGER DEFAULT 0, c_opaque INTEGER DEFAULT 0,
  c_email INTEGER DEFAULT 0, c_literal INTEGER DEFAULT 0,
  len_min INTEGER, len_max INTEGER
);
INSERT OR IGNORE INTO root_stats (id) VALUES (1);

CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v);
"#;

const KEYS_DROP: &str = "DROP TABLE IF EXISTS keys; DROP TABLE IF EXISTS fold_stats; DROP TABLE IF EXISTS keys_fts_v; \
     DROP TABLE IF EXISTS keys_fts; DROP TABLE IF EXISTS scans; DROP TABLE IF EXISTS meta;";
const NODES_DROP: &str = "DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS root_stats; DROP TABLE IF EXISTS meta;";

/// Where the index lives.
#[derive(Debug, Clone)]
pub enum Location {
    /// Persistent files, created with user-only permissions. The path names
    /// the keys file; node shards sit next to it as `<stem>.nodes<i>.sqlite`.
    File(PathBuf),
    /// In memory, gone at close. The ephemeral option for prod connections.
    Memory,
}

impl Location {
    fn shard_path(path: &Path, i: usize) -> PathBuf {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "index".into());
        path.with_file_name(format!("{stem}.nodes{i}.sqlite"))
    }

    /// All files that belong to an index at this location (existing or not).
    pub fn files(&self, shards: usize) -> Vec<PathBuf> {
        match self {
            Location::File(p) => std::iter::once(p.clone())
                .chain((0..shards).map(|i| Self::shard_path(p, i)))
                .collect(),
            Location::Memory => Vec::new(),
        }
    }
}

/// Tunables that do not change the schema.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Total SQLite page cache in megabytes across all files (CONCEPT 3.2:
    /// 64 to 256). Larger caches showed no gain in the 10M benchmark.
    pub cache_mb: u32,
    /// Node shard files. Fixed when the index is created; an existing index
    /// keeps its count.
    pub shards: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions { cache_mb: 64, shards: 4 }
    }
}

/// Persisted state of the scan that fills this index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanState {
    pub generation: i64,
    pub status: ScanStatus,
    /// The persisted `SCAN` cursor of a single-node scan. For a cluster
    /// scan (several cursors, see `cursors`) it is 0 when every node's
    /// cursor is 0 and 1 otherwise.
    pub cursor: u64,
    /// One cursor per scanned node, keyed by node id (`""` for a
    /// standalone server).
    pub cursors: Vec<(String, u64)>,
    /// Keys returned by SCAN in this generation, duplicates included.
    pub scanned: u64,
    pub dbsize: Option<u64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// `true` when the generation was a `MATCH` or `TYPE` restricted scan,
    /// which must never delete unseen keys on completion.
    pub partial: bool,
}

impl ScanState {
    /// A scan was interrupted and left cursors to continue from.
    pub fn resumable(&self) -> bool {
        self.status == ScanStatus::Running && self.cursors.iter().any(|(_, c)| *c != 0)
    }
}

/// `scan.cursor` text: a plain number for one node, `id=cursor;id=cursor`
/// for several.
pub fn parse_cursors(text: &str) -> Vec<(String, u64)> {
    if text.is_empty() {
        return Vec::new();
    }
    if !text.contains('=') {
        return vec![(String::new(), text.parse().unwrap_or(0))];
    }
    text.split(';')
        .filter_map(|part| {
            let (id, c) = part.split_once('=')?;
            Some((id.to_string(), c.parse().ok()?))
        })
        .collect()
}

pub fn format_cursors(cursors: &[(String, u64)]) -> String {
    match cursors {
        [(id, c)] if id.is_empty() => c.to_string(),
        _ => cursors.iter().map(|(id, c)| format!("{id}={c}")).collect::<Vec<_>>().join(";"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStatus {
    /// No scan has run for this generation.
    Idle,
    /// A scan was running when the state was last written; resumable.
    Running,
    Complete,
}

impl ScanStatus {
    fn as_str(self) -> &'static str {
        match self {
            ScanStatus::Idle => "idle",
            ScanStatus::Running => "running",
            ScanStatus::Complete => "complete",
        }
    }
    fn parse(s: &str) -> ScanStatus {
        match s {
            "running" => ScanStatus::Running,
            "complete" => ScanStatus::Complete,
            _ => ScanStatus::Idle,
        }
    }
}

pub struct Index {
    keys: Connection,
    shards: Vec<Connection>,
    location: Location,
    delimiter: Option<u8>,
    key_count: i64,
    /// The opt-in trigram table exists (CONCEPT 3.3).
    fts: bool,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("location", &self.location)
            .field("shards", &self.shards.len())
            .field("delimiter", &self.delimiter)
            .finish()
    }
}

impl Index {
    /// Open or create an index. Files with a foreign schema version are
    /// emptied and recreated. Node files that are out of step with the keys
    /// file (a crash between commits) are rebuilt from the keys.
    pub fn open(location: Location, options: &IndexOptions) -> Result<Index> {
        let keys = open_connection(&location, None)?;
        let mut rebuilt = ensure_schema(&keys, KEYS_SCHEMA, KEYS_DROP)?;
        let stored_shards = meta_i64(&keys, "shards")?.map(|n| n as usize);
        let n = match stored_shards {
            Some(n) if n > 0 => n,
            _ => {
                let n = options.shards.max(1);
                set_meta(&keys, "shards", &(n as i64))?;
                n
            }
        };
        let keys_cache = (i64::from(options.cache_mb) * 2 / 5).max(8);
        let shard_cache = ((i64::from(options.cache_mb) - keys_cache) / n as i64).max(8);
        apply_pragmas(&keys, keys_cache)?;
        let mut shards = Vec::with_capacity(n);
        for i in 0..n {
            let c = open_connection(&location, Some(i))?;
            rebuilt |= ensure_schema(&c, NODES_SCHEMA, NODES_DROP)?;
            apply_pragmas(&c, shard_cache)?;
            shards.push(c);
        }
        let mut index = Index {
            keys,
            shards,
            location,
            delimiter: None,
            key_count: 0,
            fts: false,
        };
        if rebuilt {
            tracing::warn!("index schema version differs; rebuilding the cache");
            index.clear()?;
        }
        index.delimiter = index.meta_i64("delimiter")?.map(|v| v as u8);
        index.key_count = index.meta_i64("key_count")?.unwrap_or(0);
        index.fts = search::has_fts_table(&index.keys)?;
        if index.fts {
            search::tune_fts_writer(&index.keys)?;
        }
        let keys_seq = index.meta_i64("merge_seq")?.unwrap_or(0);
        let out_of_step = index
            .shards
            .iter()
            .any(|s| meta_i64(s, "merge_seq").ok().flatten().unwrap_or(0) != keys_seq);
        if out_of_step {
            tracing::warn!("node files are out of step with the keys file; rebuilding the tree");
            if let Some(d) = index.delimiter {
                index.rebuild_nodes(d)?;
            } else {
                index.clear_nodes()?;
            }
        }
        Ok(index)
    }

    /// Open read-only connections to an index that another `Index` (or the
    /// scan engine's writer threads) is writing. WAL mode lets such readers
    /// proceed without ever blocking on the writer. The files must exist.
    pub fn open_reader(location: &Location, options: &IndexOptions) -> Result<Index> {
        let Location::File(_) = location else {
            return Err(Error::Config("a reader needs a file-backed index".into()));
        };
        let keys = open_connection_with(location, None, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        let n = meta_i64(&keys, "shards")?.filter(|&n| n > 0).map(|n| n as usize).unwrap_or(1);
        let keys_cache = (i64::from(options.cache_mb) * 2 / 5).max(8);
        let shard_cache = ((i64::from(options.cache_mb) - keys_cache) / n as i64).max(8);
        keys.execute_batch(&format!("PRAGMA cache_size = -{}; PRAGMA busy_timeout = 2000;", keys_cache * 1024))?;
        let mut shards = Vec::with_capacity(n);
        for i in 0..n {
            let c = open_connection_with(
                location,
                Some(i),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            c.execute_batch(&format!("PRAGMA cache_size = -{}; PRAGMA busy_timeout = 2000;", shard_cache * 1024))?;
            shards.push(c);
        }
        let mut index = Index {
            keys,
            shards,
            location: location.clone(),
            delimiter: None,
            key_count: 0,
            fts: false,
        };
        index.reload_meta()?;
        Ok(index)
    }

    /// Re-read the counters a concurrent writer may have advanced.
    pub fn reload_meta(&mut self) -> Result<()> {
        self.delimiter = self.meta_i64("delimiter")?.map(|v| v as u8);
        self.key_count = self.meta_i64("key_count")?.unwrap_or(0);
        self.fts = search::has_fts_table(&self.keys)?;
        Ok(())
    }

    /// Drop every row and start over; the cache equivalent of "clear index".
    pub fn clear(&mut self) -> Result<()> {
        let shards = self.meta_i64("shards")?.unwrap_or(self.shards.len() as i64);
        self.keys.execute_batch(KEYS_DROP)?;
        self.keys.execute_batch(KEYS_SCHEMA)?;
        set_meta(&self.keys, "schema_version", &SCHEMA_VERSION)?;
        set_meta(&self.keys, "shards", &shards)?;
        for s in &self.shards {
            s.execute_batch(NODES_DROP)?;
            s.execute_batch(NODES_SCHEMA)?;
            set_meta(s, "schema_version", &SCHEMA_VERSION)?;
        }
        self.delimiter = None;
        self.key_count = 0;
        self.fts = false;
        Ok(())
    }

    fn clear_nodes(&mut self) -> Result<()> {
        let seq = self.meta_i64("merge_seq")?.unwrap_or(0);
        for s in &self.shards {
            s.execute_batch(
                "DELETE FROM nodes; UPDATE root_stats SET children = 0, c_uuid = 0, c_ulid = 0, c_hex = 0, c_num = 0, c_date = 0, \
                 c_opaque = 0, c_email = 0, c_literal = 0, len_min = NULL, len_max = NULL;",
            )?;
            set_meta(s, "merge_seq", &seq)?;
        }
        self.keys.execute_batch("DELETE FROM fold_stats;")?;
        Ok(())
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Shard that holds the node row whose full path (parent prefix plus
    /// segment, without a trailing delimiter) is `path`. The children of one
    /// parent are spread over all shards, so listing them fans out and
    /// merges; this keeps a parent with millions of children balanced.
    pub fn shard_of(&self, path: &[u8]) -> usize {
        shard_of(path, self.shards.len())
    }

    /// The delimiter used to build `nodes`, once known.
    pub fn delimiter(&self) -> Option<u8> {
        self.delimiter
    }

    /// Fix the delimiter. Only allowed while `nodes` is empty; use
    /// [`Index::rebuild_nodes`] to change it afterwards.
    pub fn set_delimiter(&mut self, delimiter: u8) -> Result<()> {
        if let Some(d) = self.delimiter {
            if d == delimiter {
                return Ok(());
            }
            return Err(Error::Config(format!(
                "delimiter is already {:?}; rebuild nodes to change it",
                d as char
            )));
        }
        self.set_meta_i64("delimiter", i64::from(delimiter))?;
        self.delimiter = Some(delimiter);
        Ok(())
    }

    /// Number of distinct keys in the index, maintained incrementally.
    pub fn key_count(&self) -> i64 {
        self.key_count
    }

    pub fn scan_state(&self) -> Result<ScanState> {
        let cursors = parse_cursors(&self.meta_text("scan.cursor")?.unwrap_or_default());
        Ok(ScanState {
            generation: self.meta_i64("scan.generation")?.unwrap_or(0),
            status: ScanStatus::parse(&self.meta_text("scan.status")?.unwrap_or_default()),
            cursor: match cursors.as_slice() {
                [] => 0,
                [(_, c)] => *c,
                many => u64::from(many.iter().any(|(_, c)| *c != 0)),
            },
            cursors,
            scanned: self.meta_i64("scan.scanned")?.unwrap_or(0) as u64,
            dbsize: self.meta_i64("scan.dbsize")?.map(|v| v as u64),
            started_at: self.meta_i64("scan.started_at")?,
            finished_at: self.meta_i64("scanned_at")?,
            partial: self.meta_i64("scan.partial")?.unwrap_or(0) != 0,
        })
    }

    /// Open a new generation: the scan engine calls this before the first
    /// batch of a fresh (not resumed) scan.
    pub fn begin_generation(&mut self, partial: bool, dbsize: Option<u64>) -> Result<i64> {
        let generation = self.generation_seq()? + 1;
        with_tx(&self.keys, |tx| {
            set_meta(tx, "scan.generation", &generation)?;
            set_meta(tx, "scan.generation_seq", &generation)?;
            set_meta(tx, "scan.status", &ScanStatus::Running.as_str())?;
            set_meta(tx, "scan.cursor", &"0")?;
            set_meta(tx, "scan.scanned", &0i64)?;
            set_meta(tx, "scan.partial", &i64::from(partial))?;
            set_meta(tx, "scan.started_at", &now())?;
            match dbsize {
                Some(n) => set_meta(tx, "scan.dbsize", &(n as i64))?,
                None => {
                    tx.execute("DELETE FROM meta WHERE k = 'scan.dbsize'", [])?;
                }
            }
            Ok(())
        })?;
        Ok(generation)
    }

    /// Record that the scan cursor is live again (after a resume).
    pub fn mark_running(&mut self) -> Result<()> {
        self.set_meta_text("scan.status", ScanStatus::Running.as_str())
    }

    /// Mark the generation complete. For a full scan this prunes every key
    /// the generation did not see; partial scans never delete.
    pub fn complete_generation(&mut self, generation: i64) -> Result<u64> {
        let partial = self.meta_i64("scan.partial")?.unwrap_or(0) != 0;
        let removed = if partial { 0 } else { self.prune_unseen(generation, None)? };
        with_tx(&self.keys, |tx| {
            set_meta(tx, "scan.status", &ScanStatus::Complete.as_str())?;
            set_meta(tx, "scan.cursor", &"0")?;
            set_meta(tx, "scanned_at", &now())?;
            if !partial {
                Index::record_full_scan(tx, generation)?;
            }
            Ok(())
        })?;
        Ok(removed)
    }

    /// Size of the index on disk in bytes, all files included (0 in memory).
    pub fn disk_size(&self) -> u64 {
        self.location
            .files(self.shards.len())
            .iter()
            .map(|p| {
                let main = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                let wal = std::fs::metadata(format!("{}-wal", p.display())).map(|m| m.len()).unwrap_or(0);
                main + wal
            })
            .sum()
    }

    /// Fold the write-ahead logs into the main files.
    pub fn checkpoint(&self) -> Result<()> {
        self.keys.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        for s in &self.shards {
            s.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        Ok(())
    }

    pub fn meta_i64(&self, key: &str) -> Result<Option<i64>> {
        meta_i64(&self.keys, key)
    }

    pub fn meta_text(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .keys
            .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten())
    }

    pub fn set_meta_i64(&self, key: &str, value: i64) -> Result<()> {
        set_meta(&self.keys, key, &value)
    }

    pub fn set_meta_text(&self, key: &str, value: &str) -> Result<()> {
        set_meta(&self.keys, key, &value)
    }

    pub(crate) fn keys_conn(&self) -> &Connection {
        &self.keys
    }

    pub(crate) fn shard_conns(&self) -> &[Connection] {
        &self.shards
    }

    pub(crate) fn shard_conn_for(&self, parent: &[u8], segment: &[u8]) -> &Connection {
        let mut path = Vec::with_capacity(parent.len() + segment.len());
        path.extend_from_slice(parent);
        path.extend_from_slice(segment);
        &self.shards[self.shard_of(&path)]
    }
}

/// FNV-1a over the node path; stable across runs and Rust versions because
/// it is persisted implicitly in which file a row lives in.
pub(crate) fn shard_of(path: &[u8], shards: usize) -> usize {
    if shards <= 1 {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in path {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h % shards as u64) as usize
}

/// Run `f` inside BEGIN/COMMIT, rolling back on error.
pub(crate) fn with_tx<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN")?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

pub(crate) fn meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| r.get::<_, Option<i64>>(0))
        .optional()?
        .flatten())
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &dyn rusqlite::ToSql) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![key, value],
    )?;
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn apply_pragmas(conn: &Connection, cache_mb: i64) -> Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY; \
         PRAGMA cache_size = -{}; PRAGMA wal_autocheckpoint = 20000;",
        cache_mb * 1024
    ))?;
    Ok(())
}

/// Returns `true` when the file had to be emptied because of a foreign
/// schema version.
fn ensure_schema(conn: &Connection, schema: &str, drop: &str) -> Result<bool> {
    let has_meta: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    let mut rebuilt = false;
    if has_meta && meta_i64(conn, "schema_version")? != Some(SCHEMA_VERSION) {
        conn.execute_batch(drop)?;
        rebuilt = true;
    }
    conn.execute_batch(schema)?;
    set_meta(conn, "schema_version", &SCHEMA_VERSION)?;
    Ok(rebuilt)
}

fn open_connection(location: &Location, shard: Option<usize>) -> Result<Connection> {
    open_connection_with(
        location,
        shard,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

fn open_connection_with(location: &Location, shard: Option<usize>, flags: OpenFlags) -> Result<Connection> {
    match location {
        Location::Memory => Ok(Connection::open_in_memory()?),
        Location::File(path) => {
            let path = match shard {
                None => path.clone(),
                Some(i) => Location::shard_path(path, i),
            };
            if flags.contains(OpenFlags::SQLITE_OPEN_CREATE) {
                if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                    crate::private::create_dir(dir)?;
                }
                // the file exists with user-only permissions before SQLite
                // writes a byte to it; its WAL and SHM files take their
                // mode from it
                crate::private::create_file(&path)?;
                for suffix in ["-wal", "-shm", "-journal"] {
                    let mut side = path.clone().into_os_string();
                    side.push(suffix);
                    crate::private::restrict_existing(Path::new(&side))?;
                }
            }
            Ok(Connection::open_with_flags(&path, flags)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Sort;

    #[test]
    fn memory_index_opens_with_schema() {
        let idx = Index::open(Location::Memory, &IndexOptions::default()).unwrap();
        assert_eq!(idx.meta_i64("schema_version").unwrap(), Some(SCHEMA_VERSION));
        assert_eq!(idx.delimiter(), None);
        assert_eq!(idx.shard_count(), 4);
        let state = idx.scan_state().unwrap();
        assert_eq!(state.status, ScanStatus::Idle);
        assert_eq!(state.generation, 0);
    }

    #[cfg(unix)]
    #[test]
    fn index_files_and_their_directory_are_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("indexes").join("host_6379_0");
        let path = dir.join("db0.sqlite");
        let idx = Index::open(Location::File(path.clone()), &IndexOptions::default()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            assert_eq!(mode(&p), 0o600, "{}", p.display());
            seen += 1;
        }
        // the keys file, four node shards and their WAL and SHM files
        assert!(seen >= 10, "{seen} files");
        drop(idx);

        // files that an older version left readable are tightened on open
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _idx = Index::open(Location::File(path.clone()), &IndexOptions::default()).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn foreign_schema_version_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sqlite");
        {
            let idx = Index::open(Location::File(path.clone()), &IndexOptions::default()).unwrap();
            idx.set_meta_i64("schema_version", 999).unwrap();
            idx.set_meta_text("delimiter", "58").unwrap();
        }
        let idx = Index::open(Location::File(path.clone()), &IndexOptions::default()).unwrap();
        assert_eq!(idx.meta_i64("schema_version").unwrap(), Some(SCHEMA_VERSION));
        assert_eq!(idx.delimiter(), None);
        assert!(path.with_file_name("x.nodes0.sqlite").exists());
        assert!(path.with_file_name("x.nodes1.sqlite").exists());
    }

    #[test]
    fn shard_count_is_sticky() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sqlite");
        let _ = Index::open(Location::File(path.clone()), &IndexOptions { cache_mb: 16, shards: 3 }).unwrap();
        let idx = Index::open(Location::File(path), &IndexOptions { cache_mb: 16, shards: 1 }).unwrap();
        assert_eq!(idx.shard_count(), 3);
    }

    #[test]
    fn generations_round_trip() {
        let mut idx = Index::open(Location::Memory, &IndexOptions::default()).unwrap();
        let g = idx.begin_generation(false, Some(42)).unwrap();
        assert_eq!(g, 1);
        let s = idx.scan_state().unwrap();
        assert_eq!(s.status, ScanStatus::Running);
        assert_eq!(s.dbsize, Some(42));
        assert!(!s.partial);
        idx.complete_generation(g).unwrap();
        let s = idx.scan_state().unwrap();
        assert_eq!(s.status, ScanStatus::Complete);
        assert!(s.finished_at.is_some());
        assert_eq!(idx.begin_generation(true, None).unwrap(), 2);
        assert!(idx.scan_state().unwrap().partial);
    }

    #[test]
    fn reader_sees_writer_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sqlite");
        let opts = IndexOptions { cache_mb: 16, shards: 2 };
        let mut writer = Index::open(Location::File(path.clone()), &opts).unwrap();
        writer.set_delimiter(b':').unwrap();
        let mut reader = Index::open_reader(&Location::File(path), &opts).unwrap();
        assert_eq!(reader.shard_count(), 2);
        assert_eq!(reader.key_count(), 0);
        let g = writer.begin_generation(false, None).unwrap();
        writer.ingest(vec![b"a:1".to_vec(), b"b:2".to_vec()], g, 0, 2).unwrap();
        reader.reload_meta().unwrap();
        assert_eq!(reader.key_count(), 2);
        assert_eq!(reader.delimiter(), Some(b':'));
        assert_eq!(reader.children(b"", None, 10, Sort::Name).unwrap().len(), 2);
        assert!(reader.set_meta_i64("x", 1).is_err());
    }

    #[test]
    fn shard_hash_is_stable() {
        assert_eq!(shard_of(b"", 1), 0);
        assert_eq!(shard_of(b"user:", 4), shard_of(b"user:", 4));
        assert_ne!(shard_of(b"a:", 1 << 20), shard_of(b"b:", 1 << 20));
    }
}
