//! Freshness per subtree (CONCEPT 7, "on-demand index"; CONCEPT 4,
//! "subtree rescan"). A subtree scan walks `SCAN MATCH prefix*` and makes
//! the index exact for the keys under that prefix, and only for those. Its
//! record lives in the `scans` table, beside the state of the one full
//! scan in `meta`, which it never touches: an interrupted full scan still
//! resumes after any number of subtree scans.
//!
//! A prefix is *covered* when a completed scan saw every key under it: a
//! subtree scan of the prefix or of one of its ancestors, or a full scan.
//! Keys under an uncovered prefix are only those that happened to be seen
//! (a sample, a notification, a write through the app); their counts are
//! lower bounds.

use rusqlite::{OptionalExtension, params};

use super::{Index, ScanStatus, now, parse_cursors, set_meta, with_tx};
use crate::error::{Error, Result};
use crate::glob;

/// Where the scan engine persists its cursors with every merge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScanSlot {
    /// The full scan's state in `meta` (`scan.cursor`, `scan.scanned`).
    #[default]
    Global,
    /// The row of this prefix in `scans`.
    Subtree(Vec<u8>),
    /// Nowhere: a sample is not resumed.
    None,
}

/// The completed scan that makes a prefix exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// The scanned prefix that covers it; empty for a full scan.
    pub by: Vec<u8>,
    pub generation: i64,
    /// Unix seconds at which that scan completed.
    pub at: i64,
}

/// An interrupted subtree scan that can be resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeRun {
    pub generation: i64,
    pub cursors: Vec<(String, u64)>,
    pub scanned: u64,
}

/// One row of `scans`, for listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeScan {
    pub prefix: Vec<u8>,
    /// Completion time of the last complete scan of this prefix.
    pub finished_at: Option<i64>,
    /// A scan of this prefix was started and has not completed.
    pub running: bool,
}

/// `[lo, hi)` of the names that start with `prefix`.
fn range(prefix: &[u8]) -> (&[u8], Option<Vec<u8>>) {
    (prefix, glob::prefix_end(prefix))
}

/// An index written before the table existed, opened read-only.
fn no_table(e: &rusqlite::Error) -> bool {
    e.to_string().contains("no such table")
}

impl Index {
    /// A generation number above every one handed out so far, for a scan
    /// that keeps out of the full scan's state. `scan.generation` stays
    /// the generation of the full scan.
    pub fn next_generation(&mut self) -> Result<i64> {
        let g = self.generation_seq()? + 1;
        set_meta(&self.keys, "scan.generation_seq", &g)?;
        Ok(g)
    }

    pub(super) fn generation_seq(&self) -> Result<i64> {
        let full = self.meta_i64("scan.generation")?.unwrap_or(0);
        Ok(self.meta_i64("scan.generation_seq")?.unwrap_or(0).max(full))
    }

    /// Start a scan of the subtree under `prefix` from the beginning. What
    /// an earlier scan of it established stays in force until this one
    /// completes.
    pub fn begin_subtree(&mut self, prefix: &[u8]) -> Result<i64> {
        if prefix.is_empty() {
            return Err(Error::Config("a subtree scan needs a prefix; everything is a full scan".into()));
        }
        let generation = self.generation_seq()? + 1;
        with_tx(&self.keys, |tx| {
            set_meta(tx, "scan.generation_seq", &generation)?;
            tx.execute(
                "INSERT INTO scans (prefix, run_generation, run_started_at, run_cursors, run_scanned) VALUES (?1, ?2, ?3, '0', 0) \
                 ON CONFLICT(prefix) DO UPDATE SET run_generation = excluded.run_generation, \
                 run_started_at = excluded.run_started_at, run_cursors = '0', run_scanned = 0",
                params![prefix, generation, now()],
            )?;
            Ok(())
        })?;
        Ok(generation)
    }

    /// The interrupted scan of this prefix, when it got anywhere.
    pub fn subtree_run(&self, prefix: &[u8]) -> Result<Option<SubtreeRun>> {
        let row = self
            .keys
            .query_row(
                "SELECT run_generation, run_cursors, run_scanned FROM scans WHERE prefix = ?1 AND run_generation IS NOT NULL",
                params![prefix],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?)),
            )
            .optional();
        let row = match row {
            Ok(r) => r,
            Err(e) if no_table(&e) => None,
            Err(e) => return Err(e.into()),
        };
        Ok(row.and_then(|(generation, cursors, scanned)| {
            let cursors = parse_cursors(&cursors.unwrap_or_default());
            cursors.iter().any(|(_, c)| *c != 0).then_some(SubtreeRun {
                generation,
                cursors,
                scanned: scanned.unwrap_or(0) as u64,
            })
        }))
    }

    /// The scan of `prefix` in generation `generation` walked every node
    /// to the end: delete what it did not see under the prefix, and only
    /// there, and record the subtree as fresh. Records of prefixes beneath
    /// it are dropped; this one covers them.
    pub fn complete_subtree(&mut self, prefix: &[u8], generation: i64) -> Result<u64> {
        let (lo, hi) = range(prefix);
        let removed = self.prune_unseen(generation, Some((lo, hi.as_deref())))?;
        with_tx(&self.keys, |tx| {
            tx.execute(
                "DELETE FROM scans WHERE prefix > ?1 AND (?2 IS NULL OR prefix < ?2)",
                params![lo, hi],
            )?;
            tx.execute(
                "INSERT INTO scans (prefix, generation, finished_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(prefix) DO UPDATE SET generation = excluded.generation, finished_at = excluded.finished_at, \
                 run_generation = NULL, run_started_at = NULL, run_cursors = NULL, run_scanned = NULL",
                params![prefix, generation, now()],
            )?;
            Ok(())
        })?;
        Ok(removed)
    }

    /// A full scan completed: it covers everything, the subtree records
    /// have nothing left to say.
    pub(super) fn record_full_scan(tx: &rusqlite::Connection, generation: i64) -> Result<()> {
        set_meta(tx, "scan.full_at", &now())?;
        set_meta(tx, "scan.full_generation", &generation)?;
        tx.execute("DELETE FROM scans", [])?;
        Ok(())
    }

    /// When the last full scan completed, and its generation.
    fn full_coverage(&self) -> Result<Option<Coverage>> {
        let at = match self.meta_i64("scan.full_at")? {
            Some(at) => Some(at),
            // an index completed before `scan.full_at` was recorded
            None => {
                let complete = ScanStatus::parse(&self.meta_text("scan.status")?.unwrap_or_default()) == ScanStatus::Complete;
                let partial = self.meta_i64("scan.partial")?.unwrap_or(0) != 0;
                if complete && !partial { self.meta_i64("scanned_at")? } else { None }
            }
        };
        let Some(at) = at else { return Ok(None) };
        let generation = match self.meta_i64("scan.full_generation")? {
            Some(g) => g,
            None => self.meta_i64("scan.generation")?.unwrap_or(0),
        };
        Ok(Some(Coverage {
            by: Vec::new(),
            generation,
            at,
        }))
    }

    /// The newest completed scan that saw every key under `prefix`, or
    /// `None` when the index knows that subtree only in part.
    pub fn coverage(&self, prefix: &[u8]) -> Result<Option<Coverage>> {
        let mut best = self.full_coverage()?;
        let rows = (|| -> rusqlite::Result<Vec<(Vec<u8>, i64, i64)>> {
            let mut stmt = self.keys.prepare_cached(
                "SELECT prefix, generation, finished_at FROM scans \
                 WHERE finished_at IS NOT NULL AND length(prefix) <= length(?1) AND substr(?1, 1, length(prefix)) = prefix",
            )?;
            let rows = stmt.query_map(params![prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect()
        })();
        let rows = match rows {
            Ok(r) => r,
            Err(e) if no_table(&e) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        for (by, generation, at) in rows {
            if best.as_ref().is_none_or(|b| generation > b.generation) {
                best = Some(Coverage { by, generation, at });
            }
        }
        Ok(best)
    }

    /// Every subtree with a record, by prefix.
    pub fn subtree_scans(&self) -> Result<Vec<SubtreeScan>> {
        let rows = (|| -> rusqlite::Result<Vec<SubtreeScan>> {
            let mut stmt = self
                .keys
                .prepare_cached("SELECT prefix, finished_at, run_generation IS NOT NULL FROM scans ORDER BY prefix")?;
            let rows = stmt.query_map([], |r| {
                Ok(SubtreeScan {
                    prefix: r.get(0)?,
                    finished_at: r.get(1)?,
                    running: r.get(2)?,
                })
            })?;
            rows.collect()
        })();
        match rows {
            Ok(r) => Ok(r),
            Err(e) if no_table(&e) => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexOptions, Location, Sort};

    fn open() -> Index {
        let mut idx = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
        idx.set_delimiter(b':').unwrap();
        idx
    }

    fn keys(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    fn count(idx: &Index, parent: &[u8], segment: &[u8]) -> i64 {
        idx.node(parent, segment).unwrap().map(|n| n.count).unwrap_or(0)
    }

    #[test]
    fn a_subtree_scan_prunes_under_its_prefix_and_nowhere_else() {
        let mut idx = open();
        let g = idx.begin_generation(false, None).unwrap();
        idx.ingest(
            keys(&["user:1:a", "user:1:b", "user:2:a", "users:x", "order:1", "order:2"]),
            g,
            0,
            6,
        )
        .unwrap();
        idx.complete_generation(g).unwrap();
        let full = idx.coverage(b"user:").unwrap().unwrap();
        assert!(full.by.is_empty());

        // the server lost user:1:b and order:2 and gained user:3:a; only
        // the user: subtree is scanned again
        let s = idx.begin_subtree(b"user:").unwrap();
        assert!(s > g);
        idx.add(keys(&["user:1:a", "user:2:a", "user:3:a"]), s).unwrap();
        // not complete yet: the old state is still what covers the prefix
        assert_eq!(idx.coverage(b"user:").unwrap().unwrap(), full);
        assert_eq!(idx.complete_subtree(b"user:", s).unwrap(), 1);

        assert_eq!(idx.key_count(), 6);
        assert_eq!(count(&idx, b"", b"user"), 3);
        assert_eq!(count(&idx, b"user:", b"1"), 1);
        // neighbours in byte order and other namespaces are untouched
        assert_eq!(count(&idx, b"", b"users"), 1);
        assert_eq!(count(&idx, b"", b"order"), 2);
        assert_eq!(idx.children(b"", None, 10, Sort::Name).unwrap().len(), 3);

        let c = idx.coverage(b"user:").unwrap().unwrap();
        assert_eq!((&c.by[..], c.generation), (&b"user:"[..], s));
        // everything beneath is covered by it, the rest by the full scan
        assert_eq!(idx.coverage(b"user:1:").unwrap().unwrap().by, b"user:");
        assert!(idx.coverage(b"order:").unwrap().unwrap().by.is_empty());
        assert!(idx.coverage(b"users:").unwrap().unwrap().by.is_empty());
        // the full scan's own record is as it was
        let state = idx.scan_state().unwrap();
        assert_eq!((state.generation, state.status), (g, ScanStatus::Complete));
    }

    #[test]
    fn an_index_without_a_full_scan_is_covered_only_where_it_was_scanned() {
        let mut idx = open();
        // a sample: keys seen, nothing established
        let sample = idx.next_generation().unwrap();
        idx.add(keys(&["user:1", "order:1", "order:2"]), sample).unwrap();
        assert!(idx.coverage(b"").unwrap().is_none());
        assert!(idx.coverage(b"order:").unwrap().is_none());

        let s = idx.begin_subtree(b"order:").unwrap();
        idx.add(keys(&["order:2", "order:3"]), s).unwrap();
        assert_eq!(idx.complete_subtree(b"order:", s).unwrap(), 1);
        assert_eq!(idx.coverage(b"order:").unwrap().unwrap().by, b"order:");
        assert_eq!(idx.coverage(b"order:3").unwrap().unwrap().by, b"order:");
        assert!(idx.coverage(b"user:").unwrap().is_none());
        assert!(idx.coverage(b"").unwrap().is_none());
        assert!(
            idx.coverage(b"ord").unwrap().is_none(),
            "a shorter prefix reaches beyond what was scanned"
        );
        // the sampled key outside the scanned subtree stays
        assert_eq!(count(&idx, b"", b"user"), 1);
        assert_eq!(count(&idx, b"", b"order"), 2);

        // a scan of an ancestor replaces the records beneath it
        let s2 = idx.begin_subtree(b"order:2:").unwrap();
        idx.complete_subtree(b"order:2:", s2).unwrap();
        assert_eq!(idx.subtree_scans().unwrap().len(), 2);
        let s3 = idx.begin_subtree(b"order:").unwrap();
        idx.add(keys(&["order:2", "order:3"]), s3).unwrap();
        assert_eq!(idx.complete_subtree(b"order:", s3).unwrap(), 0);
        let scans = idx.subtree_scans().unwrap();
        assert_eq!(scans.len(), 1);
        assert_eq!((&scans[0].prefix[..], scans[0].running), (&b"order:"[..], false));

        // a full scan covers everything and drops the records
        let g = idx.begin_generation(false, None).unwrap();
        assert!(g > s3);
        idx.ingest(keys(&["user:1", "order:2"]), g, 0, 2).unwrap();
        assert_eq!(idx.complete_generation(g).unwrap(), 1);
        assert!(idx.subtree_scans().unwrap().is_empty());
        assert!(idx.coverage(b"order:").unwrap().unwrap().by.is_empty());
        assert!(idx.coverage(b"").unwrap().is_some());
    }

    #[test]
    fn a_subtree_scan_leaves_an_interrupted_full_scan_resumable() {
        let mut idx = open();
        let g = idx.begin_generation(false, Some(4)).unwrap();
        idx.ingest(keys(&["a:1", "b:1"]), g, 77, 2).unwrap();
        assert!(idx.scan_state().unwrap().resumable());

        let s = idx.begin_subtree(b"b:").unwrap();
        assert!(idx.subtree_run(b"b:").unwrap().is_none(), "no cursor yet, nothing to resume");
        idx.add(keys(&["b:1", "b:2"]), s).unwrap();
        idx.keys
            .execute(
                "UPDATE scans SET run_cursors = '9', run_scanned = 2 WHERE prefix = ?1",
                params![&b"b:"[..]],
            )
            .unwrap();
        let run = idx.subtree_run(b"b:").unwrap().unwrap();
        assert_eq!((run.generation, run.scanned), (s, 2));
        assert_eq!(run.cursors, vec![(String::new(), 9)]);
        idx.complete_subtree(b"b:", s).unwrap();
        assert!(idx.subtree_run(b"b:").unwrap().is_none());

        // the full scan goes on in its own generation from its own cursor
        let state = idx.scan_state().unwrap();
        assert!(state.resumable());
        assert_eq!((state.generation, state.cursor, state.scanned), (g, 77, 2));
        idx.ingest(keys(&["c:1"]), g, 0, 3).unwrap();
        // a:1 was seen by the full scan, b:2 by the newer subtree scan, c:1 just now
        assert_eq!(idx.complete_generation(g).unwrap(), 0);
        assert_eq!(idx.key_count(), 4);
        // and the next full scan is newer than both
        assert!(idx.begin_generation(false, None).unwrap() > s);
    }

    #[test]
    fn a_prefix_of_high_bytes_has_an_open_range() {
        let mut idx = open();
        let s0 = idx.next_generation().unwrap();
        idx.add(vec![b"\xff\xff:1".to_vec(), b"\xff\xff:2".to_vec(), b"\xfe:1".to_vec()], s0)
            .unwrap();
        let s = idx.begin_subtree(b"\xff\xff").unwrap();
        idx.add(vec![b"\xff\xff:2".to_vec()], s).unwrap();
        assert_eq!(idx.complete_subtree(b"\xff\xff", s).unwrap(), 1);
        assert_eq!(idx.key_count(), 2);
        assert!(idx.begin_subtree(b"").is_err());
    }
}
