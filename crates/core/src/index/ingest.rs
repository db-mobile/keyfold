//! Ingestion: keys and the materialised namespace tree.
//!
//! A merge runs in phases that the serial API here and the parallel
//! [`super::workers`] share:
//!
//! 1. keys file: sorted `INSERT OR IGNORE`, which tells which keys are new;
//!    commits with the scan cursor
//! 2. in memory: split every new key by delimiter and aggregate one delta
//!    per `(parent, segment)`, partitioned by shard and sorted
//! 3. each shard: upsert its node rows; rows that appear or vanish yield
//!    deltas for their parent's distinct-children and class counters
//! 4. each shard: apply the parent deltas routed to it (root counters live
//!    in shard 0), commit
//!
//! Counts only move when a key is actually new to, or removed from, the
//! index, so `SCAN` duplicates are harmless. Sorting matters: `SCAN` yields
//! hash order, and unsorted inserts make every row dirty its own page.
//! Every file records the merge sequence it last committed; files that
//! disagree after a crash make [`super::Index::open`] rebuild the tree.

use rusqlite::{Connection, OptionalExtension, params};

use super::batch::KeyBatch;
use super::{Index, set_meta, shard_of, with_tx};
use crate::error::{Error, Result};
use crate::segment::{SegClass, classify, numeric_value, split};

/// Result of one ingest call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestStats {
    /// Keys that were not in the index before.
    pub new: u64,
    /// Keys already present (SCAN duplicates or a rescan).
    pub existing: u64,
}

/// Longest key that gets tree rows; longer keys stay in `keys` only.
pub const MAX_TREE_KEY: usize = u16::MAX as usize;

/// One node delta, referencing a key in the batch it was derived from:
/// `parent = key[..parent_len]`, `segment = key[parent_len..seg_end]`.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub key: u32,
    pub parent_len: u16,
    pub seg_end: u16,
    /// Keys under the node that this merge adds (or removes).
    pub count: i32,
    /// +1 when a key ends exactly here, -1 when such a key is removed.
    pub leaf: i8,
}

impl Entry {
    #[inline]
    pub fn parent<'a>(&self, batch: &'a KeyBatch) -> &'a [u8] {
        &batch.key(self.key)[..self.parent_len as usize]
    }
    #[inline]
    pub fn segment<'a>(&self, batch: &'a KeyBatch) -> &'a [u8] {
        &batch.key(self.key)[self.parent_len as usize..self.seg_end as usize]
    }
}

/// Change to a parent row's distinct-children and class counters. Compact:
/// millions of these are alive during a large merge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParentDelta {
    pub children: i32,
    pub classes: [i32; 8],
    /// Shortest and longest new child segment, or `u16::MAX`/0 when none.
    pub len_min: u16,
    pub len_max: u16,
}

impl ParentDelta {
    pub const NONE: ParentDelta = ParentDelta {
        children: 0,
        classes: [0; 8],
        len_min: u16::MAX,
        len_max: 0,
    };

    fn child_added(&mut self, segment: &[u8]) {
        self.children += 1;
        self.classes[classify(segment).index()] += 1;
        let len = segment.len().min(usize::from(u16::MAX)) as u16;
        self.len_min = self.len_min.min(len);
        self.len_max = self.len_max.max(len);
    }

    fn child_removed(&mut self, segment: &[u8]) {
        self.children -= 1;
        self.classes[classify(segment).index()] -= 1;
    }

    pub fn is_empty(&self) -> bool {
        self.children == 0 && self.classes.iter().all(|&c| c == 0)
    }

    fn add(&mut self, other: &ParentDelta) {
        self.children += other.children;
        for (a, b) in self.classes.iter_mut().zip(other.classes) {
            *a += b;
        }
        self.len_min = self.len_min.min(other.len_min);
        self.len_max = self.len_max.max(other.len_max);
    }

    fn len_min_sql(&self) -> Option<i64> {
        (self.len_min != u16::MAX).then_some(i64::from(self.len_min))
    }

    fn len_max_sql(&self) -> Option<i64> {
        (self.len_min != u16::MAX).then_some(i64::from(self.len_max))
    }
}

/// A parent whose set of distinct children changed: the prefix (with its
/// trailing delimiter) is `batch.key(key)[..prefix_len]`.
#[derive(Debug, Clone, Copy)]
pub struct ParentRef {
    pub key: u32,
    pub prefix_len: u16,
    pub delta: ParentDelta,
}

impl ParentRef {
    #[inline]
    pub fn prefix<'a>(&self, batch: &'a KeyBatch) -> &'a [u8] {
        &batch.key(self.key)[..self.prefix_len as usize]
    }
}

/// Deltas routed to each shard (sorted by parent path); the root delta is
/// applied by shard 0.
#[derive(Debug, Default)]
pub struct RoutedParents {
    pub root: ParentDelta,
    pub per_shard: Vec<Vec<ParentRef>>,
}

/// `parent` of a node is the full prefix including the trailing delimiter.
/// This splits that prefix into the parent's own (grandparent, segment).
pub fn parent_row(prefix: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    if prefix.is_empty() {
        return None;
    }
    let body = &prefix[..prefix.len() - 1];
    match body.iter().rposition(|&b| b == delimiter) {
        Some(i) => Some((&body[..=i], &body[i + 1..])),
        None => Some((&[], body)),
    }
}

// ---- phase 1: keys ---------------------------------------------------------

/// Insert the batch's sorted distinct keys; returns the indices of keys that
/// were new. With `fts`, new names also go into the trigram table.
pub fn insert_keys(conn: &Connection, batch: &KeyBatch, generation: i64, fts: bool) -> Result<(Vec<u32>, IngestStats)> {
    let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO keys (name, seen_at) VALUES (?1, ?2)")?;
    let mut touch = conn.prepare_cached("UPDATE keys SET seen_at = ?2 WHERE name = ?1 AND seen_at < ?2")?;
    let mut new_idx = Vec::with_capacity(batch.sorted().len());
    let mut stats = IngestStats {
        new: 0,
        existing: (batch.len() - batch.sorted().len()) as u64,
    };
    for &i in batch.sorted() {
        let key = batch.key(i);
        if insert.execute(params![key, generation])? == 1 {
            stats.new += 1;
            new_idx.push(i);
        } else {
            stats.existing += 1;
            touch.execute(params![key, generation])?;
        }
    }
    if fts && !new_idx.is_empty() {
        let names: Vec<&[u8]> = new_idx.iter().map(|&i| batch.key(i)).collect();
        super::search::fts_insert_many(conn, &names)?;
    }
    Ok((new_idx, stats))
}

/// Delete the batch's keys; returns the indices of keys that were present.
/// With `fts`, each deleted row hands back its trigram rowid and that row
/// goes too.
pub fn delete_keys(conn: &Connection, batch: &KeyBatch, fts: bool) -> Result<Vec<u32>> {
    let mut del = conn.prepare_cached("DELETE FROM keys WHERE name = ?1 RETURNING fts_rowid")?;
    let mut removed = Vec::new();
    let mut rowids: Vec<i64> = Vec::new();
    for &i in batch.sorted() {
        let gone = del.query_row(params![batch.key(i)], |r| r.get::<_, Option<i64>>(0)).optional()?;
        if let Some(rowid) = gone {
            removed.push(i);
            if let Some(id) = rowid {
                rowids.push(id);
            }
        }
    }
    if fts && !rowids.is_empty() {
        super::search::fts_delete(conn, &mut rowids)?;
    }
    Ok(removed)
}

// ---- phase 2: deltas in memory --------------------------------------------

/// Aggregate node deltas for the given keys (`sign` +1 insert, -1 remove),
/// partitioned by shard and sorted by (parent, segment) within each.
///
/// One raw entry per key and level is bucketed by shard, then each bucket is
/// sorted on its own thread and adjacent duplicates are folded. No hash map:
/// at a million keys per merge the sort is cheaper and the memory flat.
pub fn build_entries(batch: &KeyBatch, which: &[u32], delimiter: u8, shards: usize, sign: i8) -> Vec<Vec<Entry>> {
    let mut per_shard: Vec<Vec<Entry>> = (0..shards).map(|_| Vec::with_capacity(which.len() * 2 / shards.max(1))).collect();
    let mut skipped = 0usize;
    for &ki in which {
        let key = batch.key(ki);
        if key.len() > MAX_TREE_KEY {
            skipped += 1;
            continue;
        }
        let mut parent_len = 0usize;
        let mut segs = split(key, delimiter).peekable();
        while let Some(seg) = segs.next() {
            let seg_end = parent_len + seg.len();
            let last = segs.peek().is_none();
            per_shard[shard_of(&key[..seg_end], shards)].push(Entry {
                key: ki,
                parent_len: parent_len as u16,
                seg_end: seg_end as u16,
                count: i32::from(sign),
                leaf: if last { sign } else { 0 },
            });
            parent_len = seg_end + 1;
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "keys longer than {MAX_TREE_KEY} bytes are not shown in the tree");
    }
    std::thread::scope(|scope| {
        for bucket in &mut per_shard {
            scope.spawn(move || {
                bucket.sort_unstable_by(|a, b| (a.parent(batch), a.segment(batch)).cmp(&(b.parent(batch), b.segment(batch))));
                let mut out = 0usize;
                for i in 0..bucket.len() {
                    let cur = bucket[i];
                    if out > 0 {
                        let prev = &mut bucket[out - 1];
                        if prev.parent(batch) == cur.parent(batch) && prev.segment(batch) == cur.segment(batch) {
                            prev.count += cur.count;
                            prev.leaf += cur.leaf;
                            continue;
                        }
                    }
                    bucket[out] = cur;
                    out += 1;
                }
                bucket.truncate(out);
            });
        }
    });
    per_shard
}

// ---- phase 3: node rows per shard -----------------------------------------

/// Upsert this shard's entries. Returns one delta per parent prefix whose
/// set of distinct children changed. Entries are sorted by parent, so the
/// aggregation is a streaming pass with no map.
pub fn upsert_nodes(conn: &Connection, batch: &KeyBatch, entries: &[Entry]) -> Result<Vec<ParentRef>> {
    let mut insert =
        conn.prepare_cached("INSERT OR IGNORE INTO nodes (parent, segment, leaf, count, seg_num) VALUES (?1, ?2, ?3, ?4, ?5)")?;
    let mut bump = conn.prepare_cached("UPDATE nodes SET count = count + ?3, leaf = leaf + ?4 WHERE parent = ?1 AND segment = ?2")?;
    let mut shrink = conn.prepare_cached(
        "UPDATE nodes SET count = count + ?3, leaf = max(0, leaf + ?4) WHERE parent = ?1 AND segment = ?2 RETURNING count",
    )?;
    let mut delete = conn.prepare_cached("DELETE FROM nodes WHERE parent = ?1 AND segment = ?2")?;
    let mut out: Vec<ParentRef> = Vec::new();
    let mut current: Option<ParentRef> = None;
    for e in entries {
        if e.count == 0 {
            continue;
        }
        let parent = e.parent(batch);
        let segment = e.segment(batch);
        let changed: Option<bool> = if e.count > 0 {
            let inserted = insert.execute(params![
                parent,
                segment,
                i64::from(e.leaf > 0),
                i64::from(e.count),
                numeric_value(segment)
            ])? == 1;
            if !inserted {
                bump.execute(params![parent, segment, i64::from(e.count), i64::from(e.leaf)])?;
            }
            inserted.then_some(true)
        } else {
            let count_after: Option<i64> = {
                let mut rows = shrink.query(params![parent, segment, i64::from(e.count), i64::from(e.leaf)])?;
                match rows.next()? {
                    Some(r) => Some(r.get(0)?),
                    None => None,
                }
            };
            if count_after == Some(0) {
                delete.execute(params![parent, segment])?;
                Some(false)
            } else {
                None
            }
        };
        let Some(added) = changed else { continue };
        match current.as_mut() {
            Some(c) if c.prefix(batch) == parent => {
                if added {
                    c.delta.child_added(segment);
                } else {
                    c.delta.child_removed(segment);
                }
            }
            _ => {
                if let Some(c) = current.take() {
                    out.push(c);
                }
                let mut delta = ParentDelta::NONE;
                if added {
                    delta.child_added(segment);
                } else {
                    delta.child_removed(segment);
                }
                current = Some(ParentRef {
                    key: e.key,
                    prefix_len: e.parent_len,
                    delta,
                });
            }
        }
    }
    if let Some(c) = current.take() {
        out.push(c);
    }
    out.retain(|p| !p.delta.is_empty());
    Ok(out)
}

/// Route parent deltas to the shard that owns each parent's own row; the
/// root delta is applied by shard 0.
pub fn route_parents(batch: &KeyBatch, deltas: Vec<ParentRef>, shards: usize) -> RoutedParents {
    let mut routed = RoutedParents {
        root: ParentDelta::NONE,
        per_shard: (0..shards).map(|_| Vec::new()).collect(),
    };
    for p in deltas {
        if p.prefix_len == 0 {
            routed.root.add(&p.delta);
        } else {
            let path = &p.prefix(batch)[..p.prefix_len as usize - 1];
            routed.per_shard[shard_of(path, shards)].push(p);
        }
    }
    for v in &mut routed.per_shard {
        v.sort_unstable_by(|a, b| a.prefix(batch).cmp(b.prefix(batch)));
    }
    routed
}

// ---- phase 4: parent counters ---------------------------------------------

fn counter_sql(table: &str, where_clause: &str) -> String {
    let set = SegClass::ALL
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{col} = {col} + ?{n}", col = c.column(), n = i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "UPDATE {table} SET children = children + ?1, {set}, \
         len_min = min(coalesce(len_min, ?10), ?10), len_max = max(coalesce(len_max, ?11), ?11) WHERE {where_clause}"
    )
}

/// Apply routed parent updates on one shard.
pub fn apply_parent_updates(conn: &Connection, batch: &KeyBatch, delimiter: u8, updates: &[ParentRef]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let sql = counter_sql("nodes", "parent = ?12 AND segment = ?13");
    let mut stmt = conn.prepare_cached(&sql)?;
    for u in updates {
        let Some((gp, seg)) = parent_row(u.prefix(batch), delimiter) else {
            continue;
        };
        let c = &u.delta.classes;
        stmt.execute(params![
            u.delta.children,
            c[0],
            c[1],
            c[2],
            c[3],
            c[4],
            c[5],
            c[6],
            c[7],
            u.delta.len_min_sql(),
            u.delta.len_max_sql(),
            gp,
            seg
        ])?;
    }
    Ok(())
}

/// Apply the root delta (shard 0 holds `root_stats`).
pub fn apply_root_update(conn: &Connection, delta: &ParentDelta) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    let sql = counter_sql("root_stats", "id = 1");
    let mut stmt = conn.prepare_cached(&sql)?;
    let c = &delta.classes;
    stmt.execute(params![
        delta.children,
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        delta.len_min_sql(),
        delta.len_max_sql()
    ])?;
    Ok(())
}

// ---- serial API --------------------------------------------------------------

impl Index {
    /// Add a batch of key names seen by generation `generation` and persist
    /// the scan cursor that follows the batch.
    pub fn ingest(&mut self, keys: Vec<Vec<u8>>, generation: i64, cursor: u64, scanned_total: u64) -> Result<IngestStats> {
        let batch: KeyBatch = keys.iter().map(Vec::as_slice).collect();
        self.ingest_batch(batch, generation, cursor, scanned_total)
    }

    /// [`Index::ingest`] for a batch already in arena form.
    pub fn ingest_batch(&mut self, batch: KeyBatch, generation: i64, cursor: u64, scanned_total: u64) -> Result<IngestStats> {
        self.ingest_inner(batch, generation, Some((cursor, scanned_total)))
    }

    /// Add keys outside a scan (write-through creates, server-side
    /// verification): like [`Index::ingest`] but leaves the persisted scan
    /// cursor alone, so an interrupted scan still resumes where it was.
    pub fn add(&mut self, keys: Vec<Vec<u8>>, generation: i64) -> Result<IngestStats> {
        let batch: KeyBatch = keys.iter().map(Vec::as_slice).collect();
        self.ingest_inner(batch, generation, None)
    }

    fn ingest_inner(&mut self, mut batch: KeyBatch, generation: i64, cursor: Option<(u64, u64)>) -> Result<IngestStats> {
        let delimiter = self
            .delimiter
            .ok_or_else(|| Error::Config("delimiter not set before ingest".into()))?;
        batch.normalise();
        let seq = self.meta_i64("merge_seq")?.unwrap_or(0) + 1;
        let fts = self.fts;
        let (new_idx, stats) = with_tx(&self.keys, |tx| {
            let (new_idx, stats) = insert_keys(tx, &batch, generation, fts)?;
            let key_count = self.key_count + stats.new as i64;
            set_meta(tx, "key_count", &key_count)?;
            if let Some((cursor, scanned_total)) = cursor {
                set_meta(tx, "scan.cursor", &cursor.to_string())?;
                set_meta(tx, "scan.scanned", &(scanned_total as i64))?;
            }
            set_meta(tx, "merge_seq", &seq)?;
            Ok((new_idx, stats))
        })?;
        self.key_count += stats.new as i64;
        self.apply_tree_deltas(&batch, &new_idx, delimiter, 1, seq)?;
        Ok(stats)
    }

    /// Remove keys from the index (write-through deletes, verify-on-open).
    /// Returns how many were actually present.
    pub fn remove(&mut self, keys: &[Vec<u8>]) -> Result<u64> {
        let delimiter = self.delimiter.ok_or_else(|| Error::Config("delimiter not set".into()))?;
        let mut batch: KeyBatch = keys.iter().map(Vec::as_slice).collect();
        batch.normalise();
        let seq = self.meta_i64("merge_seq")?.unwrap_or(0) + 1;
        let fts = self.fts;
        let removed = with_tx(&self.keys, |tx| {
            let removed = delete_keys(tx, &batch, fts)?;
            let key_count = self.key_count - removed.len() as i64;
            set_meta(tx, "key_count", &key_count)?;
            set_meta(tx, "merge_seq", &seq)?;
            Ok(removed)
        })?;
        self.key_count -= removed.len() as i64;
        self.apply_tree_deltas(&batch, &removed, delimiter, -1, seq)?;
        Ok(removed.len() as u64)
    }

    /// Cache a key's expiry and memory estimate (write-through of a
    /// mutation). Returns whether the key is in the index.
    pub fn set_key_meta(&mut self, name: &[u8], ttl: Option<i64>, mem: Option<i64>) -> Result<bool> {
        let n = self
            .keys
            .execute("UPDATE keys SET ttl = ?2, mem = ?3 WHERE name = ?1", params![name, ttl, mem])?;
        Ok(n > 0)
    }

    /// Phases 2 to 4 across all shards on the calling thread; each shard
    /// commits before this returns.
    fn apply_tree_deltas(&self, batch: &KeyBatch, which: &[u32], delimiter: u8, sign: i8, seq: i64) -> Result<()> {
        let shards = self.shards.len();
        let entries = build_entries(batch, which, delimiter, shards, sign);
        for s in &self.shards {
            s.execute_batch("BEGIN")?;
        }
        let result = (|| -> Result<()> {
            let mut deltas = Vec::new();
            for (s, e) in self.shards.iter().zip(&entries) {
                deltas.extend(upsert_nodes(s, batch, e)?);
            }
            let routed = route_parents(batch, deltas, shards);
            for (i, (s, u)) in self.shards.iter().zip(&routed.per_shard).enumerate() {
                apply_parent_updates(s, batch, delimiter, u)?;
                if i == 0 {
                    apply_root_update(s, &routed.root)?;
                }
                set_meta(s, "merge_seq", &seq)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                for s in &self.shards {
                    s.execute_batch("COMMIT")?;
                }
                Ok(())
            }
            Err(e) => {
                for s in &self.shards {
                    let _ = s.execute_batch("ROLLBACK");
                }
                Err(e)
            }
        }
    }

    /// Delete every key whose `seen_at` is older than `generation`, in the
    /// whole index or within `[lo, hi)` (`hi == None` is open). Called once
    /// a full scan, or the scan of a subtree, completes.
    pub(super) fn prune_unseen(&mut self, generation: i64, range: Option<(&[u8], Option<&[u8]>)>) -> Result<u64> {
        if self.delimiter.is_none() {
            return Ok(0);
        }
        let mut total = 0u64;
        let (lo, hi) = range.unwrap_or((&[], None));
        let mut after: Vec<u8> = Vec::new();
        let mut first = true;
        loop {
            let stale: Vec<Vec<u8>> = {
                let mut stmt = self.keys.prepare_cached(
                    "SELECT name FROM keys WHERE seen_at < ?1 AND (?3 OR name > ?2) AND name >= ?4 AND (?5 IS NULL OR name < ?5) \
                     ORDER BY name LIMIT 10000",
                )?;
                let rows = stmt.query_map(params![generation, after, first, lo, hi], |r| r.get::<_, Vec<u8>>(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let Some(last) = stale.last().cloned() else { break };
            first = false;
            after = last;
            total += self.remove(&stale)?;
        }
        Ok(total)
    }

    /// Rebuild `nodes` from `keys` with another delimiter, locally, without
    /// touching the server.
    pub fn rebuild_nodes(&mut self, delimiter: u8) -> Result<()> {
        self.clear_nodes()?;
        self.set_meta_i64("delimiter", i64::from(delimiter))?;
        self.delimiter = Some(delimiter);
        let seq = self.meta_i64("merge_seq")?.unwrap_or(0);
        let mut after: Option<Vec<u8>> = None;
        loop {
            let mut batch = KeyBatch::with_capacity(200_000, 200_000 * 40);
            {
                let mut stmt = self
                    .keys
                    .prepare_cached("SELECT name FROM keys WHERE (?2 OR name > ?1) ORDER BY name LIMIT 200000")?;
                let mut rows = stmt.query(params![after.as_deref().unwrap_or(&[]), after.is_none()])?;
                while let Some(r) = rows.next()? {
                    let name: Vec<u8> = r.get(0)?;
                    batch.push(&name);
                }
            }
            if batch.is_empty() {
                break;
            }
            after = Some(batch.key(batch.len() as u32 - 1).to_vec());
            batch.normalise();
            let all: Vec<u32> = batch.sorted().to_vec();
            self.apply_tree_deltas(&batch, &all, delimiter, 1, seq)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexOptions, Location, Sort};

    fn fresh() -> Index {
        let mut idx = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 3 }).unwrap();
        idx.set_delimiter(b':').unwrap();
        idx
    }

    fn keys(v: &[&str]) -> Vec<Vec<u8>> {
        v.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn parent_row_splits_prefix() {
        assert_eq!(parent_row(b"", b':'), None);
        assert_eq!(parent_row(b"user:", b':'), Some((&b""[..], &b"user"[..])));
        assert_eq!(parent_row(b"user:42:", b':'), Some((&b"user:"[..], &b"42"[..])));
        assert_eq!(parent_row(b":", b':'), Some((&b""[..], &b""[..])));
    }

    #[test]
    fn entries_are_aggregated_and_sorted() {
        let mut b: KeyBatch = keys(&["user:1:profile", "user:1:cart", "user:2:profile", "plain"])
            .iter()
            .map(Vec::as_slice)
            .collect();
        b.normalise();
        let per_shard = build_entries(&b, b.sorted(), b':', 1, 1);
        let e = &per_shard[0];
        assert_eq!(e.len(), 7);
        let user = e.iter().find(|x| x.parent(&b) == b"" && x.segment(&b) == b"user").unwrap();
        assert_eq!(user.count, 3);
        assert_eq!(user.leaf, 0);
        let plain = e.iter().find(|x| x.segment(&b) == b"plain").unwrap();
        assert_eq!(plain.leaf, 1);
        let pairs: Vec<(&[u8], &[u8])> = e.iter().map(|x| (x.parent(&b), x.segment(&b))).collect();
        let mut sorted = pairs.clone();
        sorted.sort();
        assert_eq!(pairs, sorted);
        let spread = build_entries(&b, b.sorted(), b':', 3, 1);
        assert_eq!(spread.iter().map(Vec::len).sum::<usize>(), 7);
    }

    #[test]
    fn ingest_builds_tree_counts() {
        let mut idx = fresh();
        let g = idx.begin_generation(false, None).unwrap();
        let s = idx
            .ingest(
                keys(&["user:1:profile", "user:1:cart", "user:2:profile", "user:count", "plain", "user:1"]),
                g,
                0,
                6,
            )
            .unwrap();
        assert_eq!(s, IngestStats { new: 6, existing: 0 });
        assert_eq!(idx.key_count(), 6);

        let root = idx.root().unwrap();
        assert_eq!(root.children, 2);
        assert_eq!(root.classes.literal, 2);

        let top = idx.children(b"", None, 10, Sort::Name).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].segment, b"plain");
        assert!(top[0].leaf);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[1].segment, b"user");
        assert_eq!(top[1].count, 5);
        assert_eq!(top[1].children, 3);
        assert_eq!(top[1].classes.numeric, 2);
        assert_eq!(top[1].classes.literal, 1);
        assert_eq!(top[1].len_min, Some(1));
        assert_eq!(top[1].len_max, Some(5));

        let u1 = idx.node(b"user:", b"1").unwrap().unwrap();
        assert!(u1.leaf);
        assert_eq!(u1.count, 3);
        assert_eq!(u1.children, 2);
        assert_eq!(u1.seg_num, Some(1));

        // duplicates and rescans do not move counts
        let s = idx.ingest(keys(&["user:1:profile", "plain", "plain"]), g, 7, 8).unwrap();
        assert_eq!(s, IngestStats { new: 0, existing: 3 });
        assert_eq!(idx.children(b"", None, 10, Sort::Name).unwrap()[1].count, 5);
        assert_eq!(idx.scan_state().unwrap().cursor, 7);
        assert_eq!(idx.scan_state().unwrap().scanned, 8);
        assert_eq!(idx.meta_i64("merge_seq").unwrap(), Some(2));
    }

    #[test]
    fn remove_reverses_ingest() {
        let mut idx = fresh();
        let g = idx.begin_generation(false, None).unwrap();
        idx.ingest(keys(&["user:1:profile", "user:1:cart", "user:2:profile", "user:1"]), g, 0, 4)
            .unwrap();
        assert_eq!(idx.remove(&keys(&["user:1:cart", "user:1", "missing"])).unwrap(), 2);
        assert_eq!(idx.key_count(), 2);
        let u1 = idx.node(b"user:", b"1").unwrap().unwrap();
        assert!(!u1.leaf);
        assert_eq!(u1.count, 1);
        assert_eq!(u1.children, 1);
        assert_eq!(idx.remove(&keys(&["user:1:profile"])).unwrap(), 1);
        assert!(idx.node(b"user:", b"1").unwrap().is_none());
        let user = idx.node(b"", b"user").unwrap().unwrap();
        assert_eq!(user.children, 1);
        assert_eq!(user.classes.numeric, 1);
        assert_eq!(user.count, 1);
        assert_eq!(idx.remove(&keys(&["user:2:profile"])).unwrap(), 1);
        assert_eq!(idx.root().unwrap().children, 0);
        assert_eq!(idx.children(b"", None, 10, Sort::Name).unwrap().len(), 0);
        assert_eq!(idx.table_counts().unwrap(), (0, 0));
    }

    #[test]
    fn complete_generation_prunes_unseen_keys() {
        let mut idx = fresh();
        let g1 = idx.begin_generation(false, None).unwrap();
        idx.ingest(keys(&["a:1", "a:2", "b:1"]), g1, 0, 3).unwrap();
        idx.complete_generation(g1).unwrap();
        let g2 = idx.begin_generation(false, None).unwrap();
        idx.ingest(keys(&["a:1", "c:1"]), g2, 0, 2).unwrap();
        assert_eq!(idx.complete_generation(g2).unwrap(), 2);
        assert_eq!(idx.key_count(), 2);
        let top: Vec<Vec<u8>> = idx
            .children(b"", None, 10, Sort::Name)
            .unwrap()
            .into_iter()
            .map(|n| n.segment)
            .collect();
        assert_eq!(top, vec![b"a".to_vec(), b"c".to_vec()]);
        assert_eq!(idx.node(b"", b"a").unwrap().unwrap().count, 1);
    }

    #[test]
    fn partial_generation_never_prunes() {
        let mut idx = fresh();
        let g1 = idx.begin_generation(false, None).unwrap();
        idx.ingest(keys(&["a:1", "b:1"]), g1, 0, 2).unwrap();
        idx.complete_generation(g1).unwrap();
        let g2 = idx.begin_generation(true, None).unwrap();
        idx.ingest(keys(&["a:1"]), g2, 0, 1).unwrap();
        assert_eq!(idx.complete_generation(g2).unwrap(), 0);
        assert_eq!(idx.key_count(), 2);
    }

    #[test]
    fn empty_segments_binary_and_oversized_keys() {
        let mut idx = fresh();
        let g = idx.begin_generation(false, None).unwrap();
        let mut bin = b"bin:".to_vec();
        bin.extend_from_slice(&[0, 255, 1]);
        let huge = vec![b'x'; MAX_TREE_KEY + 1];
        idx.ingest(
            vec![
                b"a::b".to_vec(),
                b":lead".to_vec(),
                b"trail:".to_vec(),
                bin.clone(),
                b"".to_vec(),
                huge,
            ],
            g,
            0,
            6,
        )
        .unwrap();
        assert_eq!(idx.key_count(), 6);
        let root = idx.children(b"", None, 10, Sort::Name).unwrap();
        let segs: Vec<&[u8]> = root.iter().map(|n| n.segment.as_slice()).collect();
        assert_eq!(segs, vec![&b""[..], b"a", b"bin", b"trail"]);
        // '' at root: both the empty key (leaf) and the ':lead' child
        assert!(root[0].leaf);
        assert_eq!(root[0].count, 2);
        assert_eq!(root[0].children, 1);
        let lead = idx.children(b":", None, 10, Sort::Name).unwrap();
        assert_eq!(lead[0].segment, b"lead");
        let mid = idx.children(b"a::", None, 10, Sort::Name).unwrap();
        assert_eq!(mid[0].segment, b"b");
        let b = idx.children(b"bin:", None, 10, Sort::Name).unwrap();
        assert_eq!(b[0].segment, &[0u8, 255, 1][..]);
        let t = idx.children(b"trail:", None, 10, Sort::Name).unwrap();
        assert_eq!(t[0].segment, b"");
        assert!(t[0].leaf);
    }

    #[test]
    fn rebuild_nodes_with_other_delimiter() {
        let mut idx = fresh();
        let g = idx.begin_generation(false, None).unwrap();
        idx.ingest(keys(&["a/1:x", "a/2:y", "b"]), g, 0, 3).unwrap();
        assert_eq!(idx.children(b"", None, 10, Sort::Name).unwrap().len(), 3);
        idx.rebuild_nodes(b'/').unwrap();
        assert_eq!(idx.delimiter(), Some(b'/'));
        let top = idx.children(b"", None, 10, Sort::Name).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].count, 2);
        assert_eq!(idx.children(b"a/", None, 10, Sort::Name).unwrap()[0].segment, b"1:x");
        assert_eq!(idx.root().unwrap().children, 2);
    }

    #[test]
    fn out_of_step_node_files_are_rebuilt_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sqlite");
        let opts = IndexOptions { cache_mb: 16, shards: 2 };
        {
            let mut idx = Index::open(Location::File(path.clone()), &opts).unwrap();
            idx.set_delimiter(b':').unwrap();
            let g = idx.begin_generation(false, None).unwrap();
            idx.ingest(keys(&["a:1", "a:2", "b:1"]), g, 0, 3).unwrap();
            // simulate a crash after the keys file committed but before the shards did
            idx.keys_conn()
                .execute("UPDATE meta SET v = v + 1 WHERE k = 'merge_seq'", [])
                .unwrap();
            idx.shard_conn_for(b"", b"a").execute("UPDATE nodes SET count = 99", []).unwrap();
        }
        let idx = Index::open(Location::File(path), &opts).unwrap();
        assert_eq!(idx.key_count(), 3);
        assert_eq!(idx.node(b"", b"a").unwrap().unwrap().count, 2);
        assert_eq!(idx.root().unwrap().children, 2);
    }
}
