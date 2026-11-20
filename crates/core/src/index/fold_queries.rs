//! Index queries behind folding: filtered child windows, counts, numeric
//! ranges and buckets, leading-character buckets, aggregated structure
//! beneath a fold, and the structural similarity sample.

use std::collections::{BTreeSet, HashMap};

use rusqlite::params;

use super::query::{NODE_COLUMNS, compare, node_from_row};
use super::{Index, MAX_WINDOW, Node, Sort};
use crate::error::Result;
use crate::segment::{SegClass, classify};

/// Narrows a child window. SQL-level bounds use the indexes; `class` and
/// `exclude_classes` are applied after the fact, so a window may scan more
/// rows than it returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildFilter {
    /// Inclusive bounds on `seg_num` (numeric buckets).
    pub seg_num: Option<(i64, i64)>,
    /// Lower bound inclusive, upper bound exclusive, on the segment bytes.
    pub segment: Option<(Vec<u8>, Option<Vec<u8>>)>,
    pub class: Option<SegClass>,
    pub exclude_classes: Vec<SegClass>,
}

impl ChildFilter {
    fn keeps(&self, segment: &[u8]) -> bool {
        if self.class.is_none() && self.exclude_classes.is_empty() {
            return true;
        }
        let c = classify(segment);
        self.class.is_none_or(|want| want == c) && !self.exclude_classes.contains(&c)
    }

    fn has_post_filter(&self) -> bool {
        self.class.is_some() || !self.exclude_classes.is_empty()
    }
}

/// Upper bound for "starts with `prefix`": the prefix followed by 0xFF.
fn prefix_upper(prefix: &[u8]) -> Vec<u8> {
    let mut hi = prefix.to_vec();
    hi.push(0xFF);
    hi.push(0xFF);
    hi
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub lo: i64,
    pub hi: i64,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharBucket {
    pub first: Vec<u8>,
    pub count: i64,
}

/// One segment that appears beneath the members of a fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldEntry {
    pub segment: Vec<u8>,
    /// Members that have this child.
    pub parents: i64,
    /// Keys under this child across all members.
    pub keys: i64,
    /// Members where this child is itself a key.
    pub leaves: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FoldSummary {
    /// Members that have any children at all.
    pub members_with_children: i64,
    pub distinct_segments: i64,
    /// Most common segments first.
    pub entries: Vec<FoldEntry>,
}

impl Index {
    /// Like [`Index::children`] with a filter. Post-filtered classes keep
    /// scanning windows until `limit` rows are found or the level ends, so
    /// listing the two literals next to a million ids reads the level once.
    pub fn children_filtered(
        &self,
        parent: &[u8],
        after: Option<&Node>,
        limit: usize,
        sort: Sort,
        filter: &ChildFilter,
    ) -> Result<Vec<Node>> {
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
        let (nlo, nhi, has_num) = match filter.seg_num {
            Some((lo, hi)) => (lo, hi, true),
            None => (0, 0, false),
        };
        let (slo, shi, has_seg, has_hi) = match &filter.segment {
            Some((lo, hi)) => (lo.clone(), hi.clone().unwrap_or_default(), true, hi.is_some()),
            None => (Vec::new(), Vec::new(), false, false),
        };
        // The numeric class is the one shape SQL knows about (`seg_num`), so
        // wanting or excluding only numeric children needs no post-filter.
        let numeric_only_excluded = filter.class.is_none() && filter.exclude_classes == [SegClass::Numeric];
        let numeric_only_wanted = filter.class == Some(SegClass::Numeric) && filter.exclude_classes.is_empty();
        let post_filter = filter.has_post_filter() && !numeric_only_excluded && !numeric_only_wanted;
        // A post-filter may discard most rows: scan in larger steps then.
        let step = if post_filter { 5000 } else { limit };
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE parent = ?1 AND (?6 OR {keyset}) \
             AND (NOT ?7 OR (seg_num IS NOT NULL AND seg_num BETWEEN ?8 AND ?9)) \
             AND (NOT ?10 OR segment >= ?11) AND (NOT ?12 OR segment < ?13) \
             AND (NOT ?14 OR seg_num IS NULL) AND (NOT ?15 OR seg_num IS NOT NULL) \
             ORDER BY {order} LIMIT ?2"
        );
        let mut out: Vec<Node> = Vec::with_capacity(limit);
        let mut cursor: Option<Node> = after.cloned();
        loop {
            let (seg, count, num, none) = match &cursor {
                Some(n) => (n.segment.clone(), n.count, n.seg_num, false),
                None => (Vec::new(), 0, None, true),
            };
            let mut rows: Vec<Node> = Vec::with_capacity(step * self.shard_count());
            for conn in self.shard_conns() {
                let mut stmt = conn.prepare_cached(&sql)?;
                let found = stmt.query_map(
                    params![
                        parent,
                        step as i64,
                        seg,
                        count,
                        num,
                        none,
                        has_num,
                        nlo,
                        nhi,
                        has_seg,
                        slo,
                        has_hi,
                        shi,
                        numeric_only_excluded,
                        numeric_only_wanted
                    ],
                    node_from_row,
                )?;
                for r in found {
                    rows.push(r?);
                }
            }
            rows.sort_by(|a, b| compare(sort, a, b));
            rows.truncate(step);
            if rows.is_empty() {
                break;
            }
            let exhausted = rows.len() < step;
            cursor = rows.last().cloned();
            for n in rows {
                if out.len() < limit && filter.keeps(&n.segment) {
                    out.push(n);
                }
            }
            if out.len() >= limit || exhausted || !post_filter {
                break;
            }
        }
        Ok(out)
    }

    /// Number of children within SQL-level bounds (post-filters ignored).
    pub fn count_children(&self, parent: &[u8], filter: &ChildFilter) -> Result<i64> {
        let (nlo, nhi, has_num) = match filter.seg_num {
            Some((lo, hi)) => (lo, hi, true),
            None => (0, 0, false),
        };
        let (slo, shi, has_seg, has_hi) = match &filter.segment {
            Some((lo, hi)) => (lo.clone(), hi.clone().unwrap_or_default(), true, hi.is_some()),
            None => (Vec::new(), Vec::new(), false, false),
        };
        let mut total = 0i64;
        for conn in self.shard_conns() {
            let mut stmt = conn.prepare_cached(
                "SELECT count(*) FROM nodes WHERE parent = ?1 \
                 AND (NOT ?2 OR (seg_num IS NOT NULL AND seg_num BETWEEN ?3 AND ?4)) \
                 AND (NOT ?5 OR segment >= ?6) AND (NOT ?7 OR segment < ?8)",
            )?;
            total += stmt.query_row(params![parent, has_num, nlo, nhi, has_seg, slo, has_hi, shi], |r| {
                r.get::<_, i64>(0)
            })?;
        }
        Ok(total)
    }

    /// Smallest and largest numeric segment under a parent, from the
    /// `nodes_num` index.
    pub fn numeric_range(&self, parent: &[u8]) -> Result<Option<(i64, i64)>> {
        let mut range: Option<(i64, i64)> = None;
        for conn in self.shard_conns() {
            let mut stmt = conn.prepare_cached("SELECT min(seg_num), max(seg_num) FROM nodes WHERE parent = ?1 AND seg_num IS NOT NULL")?;
            let (lo, hi): (Option<i64>, Option<i64>) = stmt.query_row(params![parent], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if let (Some(lo), Some(hi)) = (lo, hi) {
                range = Some(match range {
                    Some((a, b)) => (a.min(lo), b.max(hi)),
                    None => (lo, hi),
                });
            }
        }
        Ok(range)
    }

    /// Decade buckets (0..9, 10..99, ...) covering `lo..=hi`, with counts.
    /// Empty buckets are left out.
    pub fn numeric_buckets(&self, parent: &[u8], lo: i64, hi: i64) -> Result<Vec<Bucket>> {
        let mut buckets = Vec::new();
        let mut start = if lo < 0 { lo } else { 0 };
        let mut width: i64 = 10;
        if lo < 0 {
            // one bucket for everything negative
            let count = self.count_children(
                parent,
                &ChildFilter {
                    seg_num: Some((lo, -1)),
                    ..Default::default()
                },
            )?;
            if count > 0 {
                buckets.push(Bucket { lo, hi: -1, count });
            }
            start = 0;
        }
        while start <= hi {
            // decades: 0..9, 10..99, 100..999, ...
            let end = (width - 1).min(i64::MAX - 1);
            if end >= lo {
                let count = self.count_children(
                    parent,
                    &ChildFilter {
                        seg_num: Some((start.max(lo), end.min(hi))),
                        ..Default::default()
                    },
                )?;
                if count > 0 {
                    buckets.push(Bucket { lo: start, hi: end, count });
                }
            }
            if end == i64::MAX - 1 {
                break;
            }
            start = end + 1;
            width = width.saturating_mul(10);
        }
        Ok(buckets)
    }

    /// Children grouped by their first byte, for hex, uuid and ulid levels.
    pub fn leading_buckets(&self, parent: &[u8]) -> Result<Vec<CharBucket>> {
        let mut map: HashMap<Vec<u8>, i64> = HashMap::new();
        for conn in self.shard_conns() {
            let mut stmt = conn.prepare_cached("SELECT substr(segment, 1, 1), count(*) FROM nodes WHERE parent = ?1 GROUP BY 1")?;
            let rows = stmt.query_map(params![parent], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?;
            for r in rows {
                let (c, n) = r?;
                *map.entry(c).or_default() += n;
            }
        }
        let mut out: Vec<CharBucket> = map.into_iter().map(|(first, count)| CharBucket { first, count }).collect();
        out.sort_by(|a, b| a.first.cmp(&b.first));
        Ok(out)
    }

    /// The first `n` children of a class, in name order, as examples.
    pub fn sample_children(&self, parent: &[u8], class: Option<SegClass>, n: usize) -> Result<Vec<Vec<u8>>> {
        let filter = ChildFilter {
            class,
            ..Default::default()
        };
        Ok(self
            .children_filtered(parent, None, n.min(MAX_WINDOW), Sort::Name, &filter)?
            .into_iter()
            .map(|c| c.segment)
            .collect())
    }

    /// Aggregated structure one level beneath the members of a fold: for
    /// every child segment, how many members have it and how many keys it
    /// holds. Reads every `nodes` row whose parent is `prefix` plus one
    /// segment of the class, so it is a range scan of the subtree's second
    /// level; callers cache the result.
    pub fn fold_summary(&self, prefix: &[u8], delimiter: u8, class: Option<SegClass>, top: usize) -> Result<FoldSummary> {
        let hi = prefix_upper(prefix);
        let mut agg: HashMap<Vec<u8>, FoldEntry> = HashMap::new();
        let mut members: BTreeSet<Vec<u8>> = BTreeSet::new();
        for conn in self.shard_conns() {
            let mut stmt = conn.prepare_cached("SELECT parent, segment, count, leaf FROM nodes WHERE parent > ?1 AND parent < ?2")?;
            let rows = stmt.query_map(params![prefix, hi], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            for r in rows {
                let (parent, segment, count, leaf) = r?;
                let rest = &parent[prefix.len()..];
                // exactly one segment then the trailing delimiter
                let Some((&last, member)) = rest.split_last() else { continue };
                if last != delimiter || member.contains(&delimiter) {
                    continue;
                }
                if let Some(c) = class
                    && classify(member) != c
                {
                    continue;
                }
                members.insert(member.to_vec());
                let e = agg.entry(segment).or_insert_with_key(|s| FoldEntry {
                    segment: s.clone(),
                    parents: 0,
                    keys: 0,
                    leaves: 0,
                });
                e.parents += 1;
                e.keys += count;
                e.leaves += i64::from(leaf > 0);
            }
        }
        let distinct_segments = agg.len() as i64;
        let mut entries: Vec<FoldEntry> = agg.into_values().collect();
        entries.sort_by(|a, b| b.parents.cmp(&a.parents).then_with(|| a.segment.cmp(&b.segment)));
        entries.truncate(top);
        Ok(FoldSummary {
            members_with_children: members.len() as i64,
            distinct_segments,
            entries,
        })
    }

    /// Average pairwise Jaccard overlap of the child-segment sets beneath a
    /// sample of children. `None` when fewer than two sampled children have
    /// children of their own.
    pub fn similarity(&self, prefix: &[u8], delimiter: u8, sample: usize) -> Result<Option<f64>> {
        let sampled = self.children(prefix, None, sample.min(MAX_WINDOW), Sort::Name)?;
        let mut sets: Vec<BTreeSet<Vec<u8>>> = Vec::new();
        for child in sampled.iter().filter(|c| c.children > 0) {
            let kids = self.children(&child.prefix(delimiter), None, MAX_WINDOW, Sort::Name)?;
            sets.push(kids.into_iter().map(|k| k.segment).collect());
        }
        if sets.len() < 2 {
            return Ok(None);
        }
        let mut total = 0.0;
        let mut pairs = 0u64;
        for i in 0..sets.len() {
            for j in i + 1..sets.len() {
                let inter = sets[i].intersection(&sets[j]).count() as f64;
                let union = sets[i].union(&sets[j]).count() as f64;
                total += if union > 0.0 { inter / union } else { 1.0 };
                pairs += 1;
            }
        }
        Ok(Some(total / pairs as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexOptions, Location};

    fn populated() -> Index {
        let mut idx = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 3 }).unwrap();
        idx.set_delimiter(b':').unwrap();
        let g = idx.begin_generation(false, None).unwrap();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..3000u32 {
            keys.push(format!("user:{i}:profile").into_bytes());
            if i % 10 == 0 {
                keys.push(format!("user:{i}:cart").into_bytes());
            }
            if i % 2 == 0 {
                keys.push(format!("user:{i}").into_bytes());
            }
        }
        keys.push(b"user:count".to_vec());
        keys.push(b"user:index".to_vec());
        for i in 0..300u32 {
            keys.push(format!("order:{i:032x}").into_bytes());
        }
        idx.ingest(keys, g, 0, 1).unwrap();
        idx
    }

    #[test]
    fn filtered_windows_and_counts() {
        let idx = populated();
        let literals = idx
            .children_filtered(
                b"user:",
                None,
                200,
                Sort::Name,
                &ChildFilter {
                    exclude_classes: vec![SegClass::Numeric],
                    ..Default::default()
                },
            )
            .unwrap();
        let segs: Vec<&[u8]> = literals.iter().map(|n| n.segment.as_slice()).collect();
        assert_eq!(segs, vec![&b"count"[..], b"index"]);
        let f = ChildFilter {
            seg_num: Some((100, 199)),
            ..Default::default()
        };
        assert_eq!(idx.count_children(b"user:", &f).unwrap(), 100);
        let w = idx.children_filtered(b"user:", None, 200, Sort::Numeric, &f).unwrap();
        assert_eq!(w.len(), 100);
        assert_eq!(w[0].seg_num, Some(100));
        assert_eq!(w[99].seg_num, Some(199));
        let p = ChildFilter {
            segment: Some((b"12".to_vec(), Some(b"13".to_vec()))),
            ..Default::default()
        };
        assert_eq!(idx.count_children(b"user:", &p).unwrap(), 111); // 12, 120..129, 1200..1299
        let page = idx.children_filtered(b"user:", None, 5, Sort::Name, &p).unwrap();
        assert_eq!(page[0].segment, b"12");
        let next = idx.children_filtered(b"user:", Some(&page[4]), 200, Sort::Name, &p).unwrap();
        assert_eq!(next.len(), 106);
    }

    #[test]
    fn numeric_range_and_buckets() {
        let idx = populated();
        assert_eq!(idx.numeric_range(b"user:").unwrap(), Some((0, 2999)));
        let b = idx.numeric_buckets(b"user:", 0, 2999).unwrap();
        assert_eq!(
            b,
            vec![
                Bucket { lo: 0, hi: 9, count: 10 },
                Bucket { lo: 10, hi: 99, count: 90 },
                Bucket {
                    lo: 100,
                    hi: 999,
                    count: 900
                },
                Bucket {
                    lo: 1000,
                    hi: 9999,
                    count: 2000
                }
            ]
        );
        assert_eq!(idx.numeric_range(b"order:").unwrap(), None);
        let lead = idx.leading_buckets(b"order:").unwrap();
        assert_eq!(lead.len(), 1); // all 300 hex ids start with '0'
        assert_eq!(lead[0].count, 300);
        assert_eq!(
            idx.sample_children(b"user:", Some(SegClass::Literal), 3).unwrap(),
            vec![b"count".to_vec(), b"index".to_vec()]
        );
    }

    #[test]
    fn fold_summary_aggregates_second_level() {
        let idx = populated();
        let s = idx.fold_summary(b"user:", b':', Some(SegClass::Numeric), 10).unwrap();
        assert_eq!(s.members_with_children, 3000);
        assert_eq!(s.distinct_segments, 2);
        assert_eq!(s.entries[0].segment, b"profile");
        assert_eq!(s.entries[0].parents, 3000);
        assert_eq!(s.entries[0].keys, 3000);
        assert_eq!(s.entries[0].leaves, 3000);
        assert_eq!(s.entries[1].segment, b"cart");
        assert_eq!(s.entries[1].parents, 300);
        // the literal siblings have no children and do not count
        let all = idx.fold_summary(b"user:", b':', None, 10).unwrap();
        assert_eq!(all.members_with_children, 3000);
        let none = idx.fold_summary(b"order:", b':', Some(SegClass::Hex), 10).unwrap();
        assert_eq!(none.members_with_children, 0);
        assert!(none.entries.is_empty());
    }

    #[test]
    fn similarity_of_uniform_level_is_high() {
        let idx = populated();
        let s = idx.similarity(b"user:", b':', 200).unwrap().unwrap();
        assert!(s > 0.8, "{s}");
        assert_eq!(idx.similarity(b"order:", b':', 200).unwrap(), None);
    }
}
