//! Windowed reads over the index. Every call returns at most [`MAX_WINDOW`]
//! rows; the webview never sees more than that at once. Windows are keyed
//! on the last row of the previous window (keyset pagination), because an
//! offset into millions of children costs as much as reading them.

use std::cmp::Ordering;

use rusqlite::{OptionalExtension, Row as SqlRow, params};

use super::Index;
use crate::error::Result;

/// Hard cap on rows per window (CONCEPT section 2, principle 2).
pub const MAX_WINDOW: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Byte order of the segment.
    Name,
    /// Largest subtree first, then name.
    Count,
    /// Numeric segments in numeric order first, then the rest by name.
    Numeric,
}

/// Distinct direct children of a node, by shape class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassCounts {
    pub uuid: i64,
    pub ulid: i64,
    pub hex: i64,
    pub numeric: i64,
    pub date: i64,
    pub opaque: i64,
    pub email: i64,
    pub literal: i64,
}

impl ClassCounts {
    fn from_row(row: &SqlRow<'_>, first: usize) -> rusqlite::Result<ClassCounts> {
        Ok(ClassCounts {
            uuid: row.get::<_, Option<i64>>(first)?.unwrap_or(0),
            ulid: row.get::<_, Option<i64>>(first + 1)?.unwrap_or(0),
            hex: row.get::<_, Option<i64>>(first + 2)?.unwrap_or(0),
            numeric: row.get::<_, Option<i64>>(first + 3)?.unwrap_or(0),
            date: row.get::<_, Option<i64>>(first + 4)?.unwrap_or(0),
            opaque: row.get::<_, Option<i64>>(first + 5)?.unwrap_or(0),
            email: row.get::<_, Option<i64>>(first + 6)?.unwrap_or(0),
            literal: row.get::<_, Option<i64>>(first + 7)?.unwrap_or(0),
        })
    }

    /// Distinct children that have a generated-looking shape.
    pub fn generated(&self) -> i64 {
        self.uuid + self.ulid + self.hex + self.numeric + self.date + self.opaque + self.email
    }
}

/// One row of the namespace tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Prefix of the parent including its trailing delimiter; `""` at root.
    pub parent: Vec<u8>,
    pub segment: Vec<u8>,
    /// A key ends exactly here.
    pub leaf: bool,
    /// Keys under this node, this one included when `leaf`.
    pub count: i64,
    pub seg_num: Option<i64>,
    /// Distinct direct children.
    pub children: i64,
    pub classes: ClassCounts,
    pub len_min: Option<i64>,
    pub len_max: Option<i64>,
}

impl Node {
    /// The full key name if this node is a leaf.
    pub fn key(&self) -> Vec<u8> {
        let mut k = self.parent.clone();
        k.extend_from_slice(&self.segment);
        k
    }

    /// Prefix to pass to [`Index::children`] to descend into this node.
    pub fn prefix(&self, delimiter: u8) -> Vec<u8> {
        let mut p = self.key();
        p.push(delimiter);
        p
    }
}

/// One row of the flat list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: Vec<u8>,
    pub key_type: Option<i64>,
    pub ttl: Option<i64>,
    pub mem: Option<i64>,
}

/// Every `step`-th key name in byte order, for row-number addressing.
/// Rebuilt when the key count moves; between rebuilds row numbers are
/// approximate, which is what the UI labels as "≥" during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchors {
    pub step: usize,
    /// Exact row count at build time.
    pub total: i64,
    keys: Vec<Vec<u8>>,
}

impl Anchors {
    pub const DEFAULT_STEP: usize = 4096;

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Counters of the (virtual) root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootStats {
    pub children: i64,
    pub classes: ClassCounts,
    pub len_min: Option<i64>,
    pub len_max: Option<i64>,
}

pub(super) const NODE_COLUMNS: &str =
    "parent, segment, leaf, count, seg_num, children, c_uuid, c_ulid, c_hex, c_num, c_date, c_opaque, c_email, c_literal, len_min, len_max";

pub(super) fn node_from_row(row: &SqlRow<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        parent: row.get(0)?,
        segment: row.get(1)?,
        leaf: row.get::<_, i64>(2)? > 0,
        count: row.get(3)?,
        seg_num: row.get(4)?,
        children: row.get(5)?,
        classes: ClassCounts::from_row(row, 6)?,
        len_min: row.get(14)?,
        len_max: row.get(15)?,
    })
}

/// Total order of child rows under one parent for a sort mode.
pub(super) fn compare(sort: Sort, a: &Node, b: &Node) -> Ordering {
    match sort {
        Sort::Name => a.segment.cmp(&b.segment),
        Sort::Count => b.count.cmp(&a.count).then_with(|| a.segment.cmp(&b.segment)),
        Sort::Numeric => match (a.seg_num, b.seg_num) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.segment.cmp(&b.segment)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.segment.cmp(&b.segment),
        },
    }
}

impl Index {
    /// One window of direct children of the node with prefix `parent`
    /// (`""` for the root; otherwise the path including trailing delimiter),
    /// starting after the row `after` of the previous window.
    pub fn children(&self, parent: &[u8], after: Option<&Node>, limit: usize, sort: Sort) -> Result<Vec<Node>> {
        let limit = limit.min(MAX_WINDOW);
        let (order, keyset) = match sort {
            Sort::Name => ("segment", "segment > ?3"),
            Sort::Count => ("count DESC, segment", "(count < ?4 OR (count = ?4 AND segment > ?3))"),
            Sort::Numeric => (
                "seg_num IS NULL, seg_num, segment",
                "CASE WHEN ?5 IS NULL THEN (seg_num IS NULL AND segment > ?3) \
                 ELSE (seg_num IS NULL OR seg_num > ?5 OR (seg_num = ?5 AND segment > ?3)) END",
            ),
        };
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE parent = ?1 AND (?6 OR {keyset}) ORDER BY {order} LIMIT ?2");
        let (seg, count, num, none) = match after {
            Some(n) => (n.segment.as_slice(), n.count, n.seg_num, false),
            None => (&[][..], 0, None, true),
        };
        let mut rows: Vec<Node> = Vec::with_capacity(limit * self.shard_count());
        for conn in self.shard_conns() {
            let mut stmt = conn.prepare_cached(&sql)?;
            let found = stmt.query_map(params![parent, limit as i64, seg, count, num, none], node_from_row)?;
            for r in found {
                rows.push(r?);
            }
        }
        rows.sort_by(|a, b| compare(sort, a, b));
        rows.truncate(limit);
        Ok(rows)
    }

    /// A single node by (parent prefix, segment).
    pub fn node(&self, parent: &[u8], segment: &[u8]) -> Result<Option<Node>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE parent = ?1 AND segment = ?2");
        let mut stmt = self.shard_conn_for(parent, segment).prepare_cached(&sql)?;
        let mut rows = stmt.query(params![parent, segment])?;
        match rows.next()? {
            Some(r) => Ok(Some(node_from_row(r)?)),
            None => Ok(None),
        }
    }

    pub fn root(&self) -> Result<RootStats> {
        let mut stmt = self.shard_conns()[0].prepare_cached(
            "SELECT children, c_uuid, c_ulid, c_hex, c_num, c_date, c_opaque, c_email, c_literal, len_min, len_max FROM root_stats WHERE id = 1",
        )?;
        Ok(stmt.query_row([], |row| {
            Ok(RootStats {
                children: row.get(0)?,
                classes: ClassCounts::from_row(row, 1)?,
                len_min: row.get(9)?,
                len_max: row.get(10)?,
            })
        })?)
    }

    /// Sample every `step`-th key so that a row number can be turned into a
    /// bounded keyset query ([`Index::rows_at`]). Costs one short hop per
    /// anchor: about a second for 10M keys at the default step.
    pub fn build_anchors(&self, step: usize) -> Result<Anchors> {
        let step = step.max(1);
        let conn = self.keys_conn();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let first: Option<Vec<u8>> = conn
            .query_row("SELECT name FROM keys ORDER BY name LIMIT 1", [], |r| r.get(0))
            .optional()?;
        let Some(first) = first else {
            return Ok(Anchors { step, total: 0, keys });
        };
        keys.push(first);
        let mut hop = conn.prepare_cached("SELECT name FROM keys WHERE name > ?1 ORDER BY name LIMIT 1 OFFSET ?2")?;
        loop {
            let last = keys.last().unwrap();
            let next: Option<Vec<u8>> = hop.query_row(params![last, (step - 1) as i64], |r| r.get(0)).optional()?;
            match next {
                Some(k) => keys.push(k),
                None => break,
            }
        }
        let tail: i64 = conn.query_row("SELECT count(*) FROM keys WHERE name >= ?1", params![keys.last().unwrap()], |r| {
            r.get(0)
        })?;
        let total = ((keys.len() - 1) * step) as i64 + tail;
        Ok(Anchors { step, total, keys })
    }

    /// One window of rows by row number in byte order, resolved through the
    /// anchors so that no more than `anchors.step` rows are skipped.
    pub fn rows_at(&self, anchors: &Anchors, offset: usize, limit: usize) -> Result<Vec<Row>> {
        let limit = limit.min(MAX_WINDOW) as i64;
        let i = offset / anchors.step;
        let Some(anchor) = anchors.keys.get(i) else {
            return Ok(Vec::new());
        };
        let skip = (offset - i * anchors.step) as i64;
        let mut s = self
            .keys_conn()
            .prepare_cached("SELECT name, type, ttl, mem FROM keys WHERE name >= ?1 ORDER BY name LIMIT ?2 OFFSET ?3")?;
        let r = s.query_map(params![anchor, limit, skip], |r| {
            Ok(Row {
                name: r.get(0)?,
                key_type: r.get(1)?,
                ttl: r.get(2)?,
                mem: r.get(3)?,
            })
        })?;
        Ok(r.collect::<std::result::Result<_, _>>()?)
    }

    /// Keys in byte order starting after `after` (keyset pagination).
    pub fn keys_after(&self, after: Option<&[u8]>, limit: usize) -> Result<Vec<Vec<u8>>> {
        let limit = limit.min(MAX_WINDOW) as i64;
        let mut s = self
            .keys_conn()
            .prepare_cached("SELECT name FROM keys WHERE (?3 OR name > ?1) ORDER BY name LIMIT ?2")?;
        let r = s.query_map(params![after.unwrap_or(&[]), limit, after.is_none()], |r| r.get(0))?;
        Ok(r.collect::<std::result::Result<_, _>>()?)
    }

    /// Exact row counts (keys, nodes across all shards), for diagnostics and
    /// benchmarks: full table scans.
    pub fn table_counts(&self) -> Result<(i64, i64)> {
        let keys: i64 = self.keys_conn().query_row("SELECT count(*) FROM keys", [], |r| r.get(0))?;
        let mut nodes = 0i64;
        for conn in self.shard_conns() {
            nodes += conn.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))?;
        }
        Ok((keys, nodes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexOptions, Location};

    fn walk(idx: &Index, parent: &[u8], sort: Sort, window: usize) -> Vec<Node> {
        let mut all = Vec::new();
        let mut after: Option<Node> = None;
        loop {
            let page = idx.children(parent, after.as_ref(), window, sort).unwrap();
            if page.is_empty() {
                break;
            }
            after = page.last().cloned();
            all.extend(page);
        }
        all
    }

    #[test]
    fn windows_are_capped_sorted_and_keyset_paged() {
        let mut idx = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 3 }).unwrap();
        idx.set_delimiter(b':').unwrap();
        let g = idx.begin_generation(false, None).unwrap();
        let keys: Vec<Vec<u8>> = (0..500)
            .map(|i| format!("n:{i}").into_bytes())
            .chain([b"n:x".to_vec(), b"n:x:1".to_vec(), b"n:x:2".to_vec(), b"n:007".to_vec()])
            .collect();
        idx.ingest(keys.clone(), g, 0, keys.len() as u64).unwrap();
        assert_eq!(idx.children(b"n:", None, 10_000, Sort::Name).unwrap().len(), MAX_WINDOW);

        for sort in [Sort::Name, Sort::Count, Sort::Numeric] {
            let all = walk(&idx, b"n:", sort, 7);
            assert_eq!(all.len(), 502, "{sort:?}");
            let mut sorted = all.clone();
            sorted.sort_by(|a, b| compare(sort, a, b));
            assert_eq!(all, sorted, "{sort:?}");
            let mut segs: Vec<&[u8]> = all.iter().map(|n| n.segment.as_slice()).collect();
            segs.sort();
            segs.dedup();
            assert_eq!(segs.len(), 502, "{sort:?} returned a duplicate");
        }
        let by_name = idx.children(b"n:", None, 3, Sort::Name).unwrap();
        assert_eq!(
            by_name.iter().map(|n| n.segment.clone()).collect::<Vec<_>>(),
            vec![b"0".to_vec(), b"007".to_vec(), b"1".to_vec()]
        );
        let numeric = idx.children(b"n:", None, 3, Sort::Numeric).unwrap();
        assert_eq!(
            numeric.iter().map(|n| n.seg_num).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2)]
        );
        let all_numeric = walk(&idx, b"n:", Sort::Numeric, 200);
        assert_eq!(all_numeric[7].segment, b"007");
        assert_eq!(all_numeric[8].segment, b"7");
        assert_eq!(all_numeric[9].segment, b"8");
        assert_eq!(all_numeric.last().unwrap().segment, b"x");
        let by_count = idx.children(b"n:", None, 1, Sort::Count).unwrap();
        assert_eq!(by_count[0].segment, b"x");
        assert_eq!(by_count[0].count, 3);
        assert_eq!(by_count[0].prefix(b':'), b"n:x:");

        let anchors = idx.build_anchors(100).unwrap();
        assert_eq!(anchors.total, 504);
        assert_eq!(anchors.len(), 6);
        let all = idx.keys_after(None, 200).unwrap();
        let mut by_offset = Vec::new();
        for off in [0usize, 1, 99, 100, 101, 250, 503] {
            let rows = idx.rows_at(&anchors, off, 1).unwrap();
            by_offset.push(rows[0].name.clone());
        }
        let expect: Vec<Vec<u8>> = [0usize, 1, 99, 100, 101].iter().map(|&i| all[i].clone()).collect();
        assert_eq!(&by_offset[..5], &expect[..]);
        assert_eq!(idx.rows_at(&anchors, 503, 10).unwrap().len(), 1);
        assert!(idx.rows_at(&anchors, 504, 10).unwrap().is_empty());
        assert!(idx.rows_at(&anchors, 10_000, 10).unwrap().is_empty());
        let window = idx.rows_at(&anchors, 150, 200).unwrap();
        assert_eq!(window.len(), 200);
        assert!(window.windows(2).all(|w| w[0].name < w[1].name));
        let empty = Index::open(Location::Memory, &IndexOptions { cache_mb: 8, shards: 1 }).unwrap();
        let a = empty.build_anchors(10).unwrap();
        assert!(a.is_empty());
        assert_eq!(a.total, 0);
        assert!(empty.rows_at(&a, 0, 10).unwrap().is_empty());
        let page = idx.keys_after(None, 2).unwrap();
        assert_eq!(page, vec![b"n:0".to_vec(), b"n:007".to_vec()]);
        let next = idx.keys_after(Some(b"n:1"), 1).unwrap();
        assert_eq!(next, vec![b"n:10".to_vec()]);
        assert_eq!(idx.table_counts().unwrap().0, 504);
    }
}
