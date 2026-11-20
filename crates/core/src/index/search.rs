//! Search queries over the keys file and the opt-in trigram table
//! (CONCEPT 3.3, 5.3). Every call returns a window of at most
//! [`MAX_WINDOW`] matches in byte order, keyed on the last name of the
//! previous window, and stops early when the cancel flag is raised or the
//! connection is interrupted.

use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode, InterruptHandle, OptionalExtension, params, params_from_iter};

use super::query::MAX_WINDOW;
use super::{Index, Row, set_meta, with_tx};
use crate::error::{Error, Result};
use crate::search::Plan;

/// Candidate rows between two polls of the cancel flag.
const POLL_EVERY: u64 = 1024;

/// Keys copied per transaction while the trigram table is built.
const FTS_BUILD_BATCH: usize = 50_000;

/// Pending-terms hash of a writer connection while the trigram table
/// exists: FTS5 flushes a segment whenever the hash is full, and its 1 MB
/// default means a segment every few hundred keys during a scan.
const FTS_WRITER_HASH: i64 = 16 << 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPage {
    pub rows: Vec<Row>,
    /// The index has no further candidates after the last row.
    pub done: bool,
    /// Stopped by a cancel flag or an interrupt; `rows` is what was found
    /// before that.
    pub cancelled: bool,
    /// Candidates came from the trigram table.
    pub via_fts: bool,
    /// Candidate rows read from the index for this window.
    pub examined: u64,
}

/// Is `e` the error an interrupted statement raises?
fn interrupted(e: &Error) -> bool {
    matches!(e, Error::Index(rusqlite::Error::SqliteFailure(f, _)) if f.code == ErrorCode::OperationInterrupted)
}

/// The trigram table serves a query only when the rarest trigram of its
/// literal occurs in fewer than one key in this many: for common trigrams
/// the posting lists cost more than the plain `LIKE` scan (CONCEPT 3.3).
const FTS_SELECTIVITY: i64 = 20;

pub(crate) fn has_fts_table(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'keys_fts'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// Per-connection FTS5 settings for a connection that will insert keys.
pub(crate) fn tune_fts_writer(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "INSERT INTO keys_fts(keys_fts, rank) VALUES ('hashsize', {FTS_WRITER_HASH});"
    ))?;
    Ok(())
}

/// Add names to the trigram table and record each name's rowid on its
/// `keys` row, so a later delete is a point lookup (equality on an FTS5
/// column is a full scan, and a `GLOB` through the trigram index costs
/// milliseconds per name). Rowids are automatic, hence ascending: FTS5
/// keeps filling its pending-terms hash instead of flushing a segment per
/// row, which an out-of-order rowid forces, and sequential rowids keep the
/// doclist deltas one byte wide.
pub(crate) fn fts_insert_many(conn: &Connection, names: &[&[u8]]) -> Result<()> {
    let mut insert = conn.prepare_cached("INSERT INTO keys_fts (name) VALUES (?1)")?;
    let mut record = conn.prepare_cached("UPDATE keys SET fts_rowid = ?1 WHERE name = ?2")?;
    for name in names {
        insert.execute(params![name])?;
        record.execute(params![conn.last_insert_rowid(), name])?;
    }
    Ok(())
}

/// Remove rows from the trigram table by rowid, in rowid order.
pub(crate) fn fts_delete(conn: &Connection, rowids: &mut Vec<i64>) -> Result<()> {
    rowids.sort_unstable();
    rowids.dedup();
    let mut del = conn.prepare_cached("DELETE FROM keys_fts WHERE rowid = ?1")?;
    for id in rowids.iter() {
        del.execute(params![id])?;
    }
    Ok(())
}

impl Index {
    /// Whether the trigram table exists in this index (as of the last
    /// open or [`Index::reload_meta`]).
    pub fn has_fts(&self) -> bool {
        self.fts
    }

    /// A handle that aborts whatever statement the keys connection is
    /// running, from any thread. Searches report that as `cancelled`.
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.keys.get_interrupt_handle()
    }

    /// One window of matches in byte order, after `after`. One streaming
    /// statement yields candidates in order; the matcher filters them and
    /// the statement is dropped once `limit` matches are in hand, so the
    /// work is proportional to the candidates examined, not to the match
    /// set (except that the trigram path must sort its matches first).
    pub fn search(&self, plan: &Plan, after: Option<&[u8]>, limit: usize, cancel: &AtomicBool) -> Result<SearchPage> {
        let limit = limit.min(MAX_WINDOW);
        let mut page = SearchPage::default();
        let fts_term = self.fts_term(plan)?;
        page.via_fts = fts_term.is_some();
        let result = (|| -> Result<()> {
            let (sql, params) = self.candidate_sql(plan, after, fts_term.as_deref(), true);
            let mut stmt = self.keys.prepare_cached(&sql)?;
            let mut rows = stmt.query(params_from_iter(params))?;
            loop {
                if page.examined % POLL_EVERY == 0 && cancel.load(Ordering::Relaxed) {
                    page.cancelled = true;
                    return Ok(());
                }
                let Some(r) = rows.next()? else {
                    page.done = true;
                    return Ok(());
                };
                page.examined += 1;
                let name = r.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?;
                if plan.matcher.matches(name) {
                    page.rows.push(row_from(r)?);
                    if page.rows.len() == limit {
                        return Ok(());
                    }
                }
            }
        })();
        match result {
            Ok(()) => Ok(page),
            Err(e) if interrupted(&e) => {
                page.cancelled = true;
                Ok(page)
            }
            Err(e) => Err(e),
        }
    }

    /// Exact number of matches, or `None` when cancelled. Substring queries
    /// are one aggregate statement; globs and regexes stream the candidates
    /// (unordered) through the matcher.
    pub fn search_count(&self, plan: &Plan, cancel: &AtomicBool) -> Result<Option<i64>> {
        let fts_term = self.fts_term(plan)?;
        let result = (|| -> Result<Option<i64>> {
            if plan.like_is_exact {
                let n: i64 = if let Some(term) = &fts_term {
                    let mut s = self
                        .keys
                        .prepare_cached("SELECT count(*) FROM keys_fts WHERE keys_fts MATCH ?1 AND name LIKE ?2")?;
                    s.query_row(params![term, plan.like], |r| r.get(0))?
                } else {
                    let mut s = self.keys.prepare_cached(
                        "SELECT count(*) FROM keys WHERE name >= ?1 AND (?2 IS NULL OR name < ?2) AND name LIKE ?3 ESCAPE '\\'",
                    )?;
                    s.query_row(params![plan.lo, plan.hi, plan.like], |r| r.get(0))?
                };
                return Ok(Some(n));
            }
            let (sql, params) = self.candidate_sql(plan, None, fts_term.as_deref(), false);
            let mut stmt = self.keys.prepare_cached(&sql)?;
            let mut rows = stmt.query(params_from_iter(params))?;
            let mut total = 0i64;
            let mut examined = 0u64;
            while let Some(r) = rows.next()? {
                examined += 1;
                if examined.is_multiple_of(POLL_EVERY) && cancel.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                if plan.matcher.matches(r.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?) {
                    total += 1;
                }
            }
            Ok(Some(total))
        })();
        match result {
            Err(e) if interrupted(&e) => Ok(None),
            other => other,
        }
    }

    /// Should this query go through the trigram table, and on which term?
    /// FTS5's own `LIKE` optimisation intersects every trigram of the
    /// pattern, so a pattern with one rare and six common trigrams costs
    /// more than the plain scan. Instead the rarest trigram (looked up in
    /// the `fts5vocab` view) is matched alone and `LIKE` filters its
    /// postings; that is worth it only when the trigram is selective.
    fn fts_term(&self, plan: &Plan) -> Result<Option<String>> {
        if !self.fts {
            return Ok(None);
        }
        let Some(literal) = plan.fts_literal.as_deref() else {
            return Ok(None);
        };
        let folded: Vec<char> = literal.to_lowercase().chars().collect();
        let mut s = self.keys.prepare_cached("SELECT doc FROM keys_fts_v WHERE term = ?1")?;
        let mut rarest: Option<(i64, String)> = None;
        for w in folded.windows(3) {
            let term: String = w.iter().collect();
            let docs: i64 = s.query_row(params![term], |r| r.get(0)).optional()?.unwrap_or(0);
            if rarest.as_ref().is_none_or(|(d, _)| docs < *d) {
                rarest = Some((docs, term));
            }
        }
        Ok(rarest
            .filter(|(docs, _)| docs.saturating_mul(FTS_SELECTIVITY) < self.key_count.max(1))
            .map(|(_, term)| format!("\"{}\"", term.replace('"', "\"\""))))
    }

    /// The candidate statement and its parameters. The lower bound is a
    /// single comparison the B-tree can seek to, so a window continues
    /// where the previous one stopped. The trigram path matches the rarest
    /// trigram, filters its rows with the `LIKE` pattern and joins the keys
    /// row for the metadata columns.
    fn candidate_sql(&self, plan: &Plan, after: Option<&[u8]>, fts_term: Option<&str>, ordered: bool) -> (String, Vec<SqlValue>) {
        let (op, bound): (&str, &[u8]) = match after {
            Some(a) if a >= plan.lo.as_slice() => (">", a),
            _ => (">=", &plan.lo),
        };
        let order = if ordered { " ORDER BY 1" } else { "" };
        let blob = |b: Option<&[u8]>| b.map_or(SqlValue::Null, |b| SqlValue::Blob(b.to_vec()));
        let mut params = vec![SqlValue::Blob(bound.to_vec()), blob(plan.hi.as_deref()), blob(plan.like.as_deref())];
        let sql = match fts_term {
            Some(term) => {
                params.push(SqlValue::Text(term.to_string()));
                format!(
                    "SELECT k.name, k.type, k.ttl, k.mem FROM keys_fts f JOIN keys k ON k.name = f.name \
                     WHERE keys_fts MATCH ?4 AND f.name LIKE ?3 AND f.name {op} ?1 AND (?2 IS NULL OR f.name < ?2){order}"
                )
            }
            None => format!(
                "SELECT name, type, ttl, mem FROM keys WHERE name {op} ?1 AND (?2 IS NULL OR name < ?2) \
                 AND (?3 IS NULL OR name LIKE ?3 ESCAPE '\\'){order}"
            ),
        };
        (sql, params)
    }

    /// Create the trigram table and fill it from `keys`, in batches, with
    /// no server traffic. `progress(done, total)` is called after every
    /// batch and returns `false` to abort, which drops the half-built
    /// table again. Returns `true` when the table is complete.
    pub fn build_fts(&mut self, progress: &mut dyn FnMut(i64, i64) -> bool) -> Result<bool> {
        if self.fts {
            return Ok(true);
        }
        // `detail = none`: no positions, which `LIKE` through the trigram
        // index does not need, and a fraction of the build time of a full
        // table. Bulk load in batches of 50k names read in name order, each
        // name's rowid written back to its `keys` row, with a 64 MB
        // pending-terms hash, a larger page cache for the duration and one
        // `optimize` at the end; those two settings are per connection. The
        // `fts5vocab` view answers how common a trigram is.
        let cache: i64 = self.keys.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
        self.keys.execute_batch(
            "DROP TABLE IF EXISTS keys_fts_v; DROP TABLE IF EXISTS keys_fts; \
             CREATE VIRTUAL TABLE keys_fts USING fts5(name, tokenize = 'trigram', detail = 'none'); \
             CREATE VIRTUAL TABLE keys_fts_v USING fts5vocab('keys_fts', 'row'); \
             INSERT INTO keys_fts(keys_fts, rank) VALUES ('hashsize', 67108864); \
             PRAGMA cache_size = -262144;",
        )?;
        let restore = |conn: &Connection| conn.execute_batch(&format!("PRAGMA cache_size = {cache};"));
        let total = self.key_count;
        let mut done = 0i64;
        let mut after: Option<Vec<u8>> = None;
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(FTS_BUILD_BATCH);
        loop {
            names.clear();
            {
                let mut s = self
                    .keys
                    .prepare_cached("SELECT name FROM keys WHERE (?1 IS NULL OR name > ?1) ORDER BY name LIMIT ?2")?;
                let rows = s.query_map(params![after, FTS_BUILD_BATCH as i64], |r| r.get::<_, Vec<u8>>(0))?;
                for r in rows {
                    names.push(r?);
                }
            }
            if names.is_empty() {
                break;
            }
            with_tx(&self.keys, |tx| {
                let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
                fts_insert_many(tx, &refs)
            })?;
            done += names.len() as i64;
            after = names.last().cloned();
            if !progress(done, total) {
                self.keys
                    .execute_batch("DROP TABLE IF EXISTS keys_fts_v; DROP TABLE IF EXISTS keys_fts;")?;
                restore(&self.keys)?;
                return Ok(false);
            }
            if names.len() < FTS_BUILD_BATCH {
                break;
            }
        }
        self.keys.execute_batch("INSERT INTO keys_fts(keys_fts) VALUES ('optimize');")?;
        tune_fts_writer(&self.keys)?;
        restore(&self.keys)?;
        set_meta(&self.keys, "fts", &1i64)?;
        self.fts = true;
        progress(done, total);
        Ok(true)
    }

    /// Drop the trigram table and give the space back to the file system.
    pub fn drop_fts(&mut self) -> Result<()> {
        self.keys
            .execute_batch("DROP TABLE IF EXISTS keys_fts_v; DROP TABLE IF EXISTS keys_fts;")?;
        set_meta(&self.keys, "fts", &0i64)?;
        self.fts = false;
        self.keys.execute_batch("VACUUM;")?;
        Ok(())
    }
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        name: r.get(0)?,
        key_type: r.get(1)?,
        ttl: r.get(2)?,
        mem: r.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexOptions, Location};
    use crate::search::{Query, parse};

    fn filled() -> Index {
        let mut idx = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards: 2 }).unwrap();
        idx.set_delimiter(b':').unwrap();
        let g = idx.begin_generation(false, None).unwrap();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..3000 {
            keys.push(format!("user:{i}:profile").into_bytes());
            keys.push(format!("user:{i}:cart").into_bytes());
        }
        for i in 0..500 {
            keys.push(format!("order:{i:04x}").into_bytes());
            keys.push(format!("Session:{i}").into_bytes());
            keys.push(format!("event:{i}:session").into_bytes());
        }
        keys.push(b"odd:a%b_c".to_vec());
        keys.push(b"odd:a\\b".to_vec());
        keys.push(b"odd:\xff\xfe".to_vec());
        idx.ingest(keys, g, 0, 0).unwrap();
        idx
    }

    fn all(idx: &Index, input: &str) -> Vec<Vec<u8>> {
        let plan = Plan::new(parse(input)).unwrap();
        let cancel = AtomicBool::new(false);
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = idx.search(&plan, after.as_deref(), 150, &cancel).unwrap();
            assert!(!page.cancelled);
            after = page.rows.last().map(|r| r.name.clone());
            out.extend(page.rows.into_iter().map(|r| r.name));
            if page.done {
                break;
            }
        }
        assert!(out.windows(2).all(|w| w[0] < w[1]), "{input}: not in byte order or duplicated");
        let count = idx.search_count(&plan, &cancel).unwrap().unwrap();
        assert_eq!(count as usize, out.len(), "{input}: count disagrees with the walk");
        out
    }

    #[test]
    fn glob_substring_and_regex_agree_with_a_plain_filter() {
        let idx = filled();
        let brute = |f: &dyn Fn(&[u8]) -> bool| -> Vec<Vec<u8>> {
            let mut v: Vec<Vec<u8>> = idx.keys_after(None, 200).unwrap().into_iter().chain(std::iter::empty()).collect();
            // keys_after is windowed; walk it fully
            let mut after = v.last().cloned();
            while let Some(a) = after.clone() {
                let page = idx.keys_after(Some(&a), 200).unwrap();
                after = page.last().cloned();
                v.extend(page);
            }
            v.retain(|k| f(k));
            v
        };
        assert_eq!(all(&idx, "user:42:*"), brute(&|k| k.starts_with(b"user:42:")));
        assert_eq!(all(&idx, "user:4?:cart").len(), 10);
        assert_eq!(all(&idx, "*:cart").len(), 3000);
        assert_eq!(
            all(&idx, "session"),
            brute(&|k| k.to_ascii_lowercase().windows(7).any(|w| w == b"session"))
        );
        assert_eq!(all(&idx, "session").len(), 1000);
        assert_eq!(all(&idx, "Session:1").len(), 111);
        assert_eq!(all(&idx, "a%b"), vec![b"odd:a%b_c".to_vec()]);
        assert_eq!(all(&idx, "a\\b"), vec![b"odd:a\\b".to_vec()]);
        assert_eq!(all(&idx, "odd:*").len(), 3);
        assert_eq!(all(&idx, "/^user:4\\d:cart$/").len(), 10);
        assert_eq!(all(&idx, "/profile$/").len(), 3000);
        assert_eq!(all(&idx, "/^order:0[0-9a-f]{3}$/").len(), 500);
        assert_eq!(all(&idx, "nothing-like-this").len(), 0);
        assert_eq!(all(&idx, "").len(), 7503);
    }

    #[test]
    fn pages_are_capped_and_cancellable() {
        let idx = filled();
        let plan = Plan::new(parse("user:*")).unwrap();
        let cancel = AtomicBool::new(false);
        let page = idx.search(&plan, None, 10_000, &cancel).unwrap();
        assert_eq!(page.rows.len(), MAX_WINDOW);
        assert!(!page.done);
        cancel.store(true, Ordering::Relaxed);
        let page = idx.search(&plan, None, 10, &cancel).unwrap();
        assert!(page.cancelled);
        assert!(page.rows.is_empty());
        assert_eq!(idx.search_count(&plan, &cancel).unwrap(), None);
        // a substring count is a single statement and ignores the flag
        let plan = Plan::new(parse("cart")).unwrap();
        assert_eq!(idx.search_count(&plan, &cancel).unwrap(), Some(3000));
    }

    #[test]
    fn trigram_table_gives_the_same_answers_and_follows_writes() {
        let mut idx = filled();
        assert!(!idx.has_fts());
        let before = all(&idx, "session");
        let before_glob = all(&idx, "*ssion*");
        let mut calls = 0;
        assert!(
            idx.build_fts(&mut |done, total| {
                calls += 1;
                assert!(done <= total.max(done));
                true
            })
            .unwrap()
        );
        assert!(calls >= 1);
        assert!(idx.has_fts());
        let cancel = AtomicBool::new(false);
        // `session` is in 13 % of the keys: common trigrams stay on the scan
        let plan = Plan::new(parse("session")).unwrap();
        assert!(!idx.search(&plan, None, 50, &cancel).unwrap().via_fts);
        let plan = Plan::new(parse("Session:1")).unwrap();
        let page = idx.search(&plan, None, 50, &cancel).unwrap();
        assert!(page.via_fts);
        assert_eq!(page.rows.len(), 50);
        assert!(
            idx.search(&Plan::new(parse("*ssion:4*")).unwrap(), None, 50, &cancel)
                .unwrap()
                .via_fts
        );
        assert!(!idx.search(&Plan::new(parse("*a%b*")).unwrap(), None, 50, &cancel).unwrap().via_fts);
        assert_eq!(all(&idx, "session"), before);
        assert_eq!(all(&idx, "*ssion*"), before_glob);
        assert_eq!(all(&idx, "Session:1").len(), 111);
        // short terms cannot use trigrams and fall back to the scan
        let short = Plan::new(parse("ab")).unwrap();
        assert!(!idx.search(&short, None, 10, &cancel).unwrap().via_fts);
        // writes keep the table in step
        let g = idx.scan_state().unwrap().generation;
        idx.ingest(vec![b"fresh:session:1".to_vec()], g, 0, 0).unwrap();
        assert_eq!(all(&idx, "session").len(), before.len() + 1);
        idx.remove(&[b"fresh:session:1".to_vec(), b"event:1:session".to_vec()]).unwrap();
        assert_eq!(all(&idx, "session").len(), before.len() - 1);
        let n: i64 = idx
            .keys_conn()
            .query_row("SELECT count(*) FROM keys_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, idx.key_count());
        // aborting a build leaves no table behind
        idx.drop_fts().unwrap();
        assert!(!idx.has_fts());
        assert!(!idx.build_fts(&mut |_, _| false).unwrap());
        assert!(!has_fts_table(idx.keys_conn()).unwrap());
        assert_eq!(all(&idx, "session").len(), before.len() - 1);
        let _ = Query {
            mode: crate::search::Mode::Glob,
            term: vec![],
        };
    }

    #[test]
    fn reopened_index_remembers_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sqlite");
        {
            let mut idx = Index::open(Location::File(path.clone()), &IndexOptions { cache_mb: 8, shards: 1 }).unwrap();
            idx.set_delimiter(b':').unwrap();
            let g = idx.begin_generation(false, None).unwrap();
            idx.ingest(vec![b"a:session".to_vec()], g, 0, 0).unwrap();
            idx.build_fts(&mut |_, _| true).unwrap();
        }
        let idx = Index::open(Location::File(path.clone()), &IndexOptions { cache_mb: 8, shards: 1 }).unwrap();
        assert!(idx.has_fts());
        let reader = Index::open_reader(&Location::File(path), &IndexOptions { cache_mb: 8, shards: 1 }).unwrap();
        assert!(reader.has_fts());
        assert_eq!(all(&reader, "session").len(), 1);
    }
}
