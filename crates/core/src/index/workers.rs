//! Parallel ingestion: one thread per index file, pipelined.
//!
//! [`Index::into_workers`] moves every connection to its own thread; the
//! coordinator ([`Workers::merge`]) drives the phases from [`super::ingest`]
//! across them and [`Workers::join`] hands the `Index` back.
//!
//! The keys file commits a merge (with the scan cursor) as soon as its
//! inserts are done; the shards upsert that merge's tree rows while the
//! coordinator already inserts the next batch's keys. Shard transactions are
//! closed when the following merge arrives or at `join`. Each file records
//! the merge sequence it committed; after a crash the files can disagree,
//! and [`Index::open`] then rebuilds the tree from the keys.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rusqlite::Connection;

use super::batch::KeyBatch;
use super::ingest::{self, Entry, IngestStats, ParentDelta, ParentRef};
use super::{Index, Location, ScanSlot, set_meta};
use crate::error::{Error, Result};

type Reply<T> = Sender<Result<T>>;

enum KeysJob {
    SetDelimiter(u8, Reply<()>),
    Merge {
        batch: Arc<KeyBatch>,
        generation: i64,
        slot: ScanSlot,
        cursor: String,
        scanned: u64,
        seq: i64,
        key_count: i64,
        reply: Reply<(Vec<u32>, IngestStats)>,
    },
    Finish(Sender<Connection>),
}

enum ShardJob {
    Upsert {
        batch: Arc<KeyBatch>,
        entries: Vec<Entry>,
        reply: Reply<Vec<ParentRef>>,
    },
    Commit {
        batch: Arc<KeyBatch>,
        delimiter: u8,
        updates: Vec<ParentRef>,
        root: Option<ParentDelta>,
        seq: i64,
        reply: Reply<()>,
    },
    Finish(Sender<Connection>),
}

struct Pending {
    seq: i64,
    batch: Arc<KeyBatch>,
    replies: Vec<Receiver<Result<Vec<ParentRef>>>>,
    started: Instant,
}

/// The index split into writer threads.
pub struct Workers {
    keys: Sender<KeysJob>,
    shards: Vec<Sender<ShardJob>>,
    handles: Vec<JoinHandle<()>>,
    location: Location,
    delimiter: Option<u8>,
    key_count: i64,
    fts: bool,
    seq: i64,
    pending: Option<Pending>,
}

impl Index {
    /// Move the connections to writer threads for parallel ingestion.
    pub fn into_workers(self) -> Result<Workers> {
        crate::sysinfo::tune_allocator();
        let seq = self.meta_i64("merge_seq")?.unwrap_or(0);
        let Index {
            keys,
            shards,
            location,
            delimiter,
            key_count,
            fts,
        } = self;
        let mut handles = Vec::with_capacity(shards.len() + 1);
        let (ktx, krx) = channel::<KeysJob>();
        handles.push(
            thread::Builder::new()
                .name("index-keys".into())
                .spawn(move || keys_thread(keys, krx, fts))
                .map_err(Error::Io)?,
        );
        let mut stx = Vec::with_capacity(shards.len());
        for (i, conn) in shards.into_iter().enumerate() {
            let (tx, rx) = channel::<ShardJob>();
            handles.push(
                thread::Builder::new()
                    .name(format!("index-nodes{i}"))
                    .spawn(move || shard_thread(conn, rx))
                    .map_err(Error::Io)?,
            );
            stx.push(tx);
        }
        Ok(Workers {
            keys: ktx,
            shards: stx,
            handles,
            location,
            delimiter,
            key_count,
            fts,
            seq,
            pending: None,
        })
    }
}

impl Workers {
    pub fn delimiter(&self) -> Option<u8> {
        self.delimiter
    }

    pub fn key_count(&self) -> i64 {
        self.key_count
    }

    pub fn set_delimiter(&mut self, delimiter: u8) -> Result<()> {
        if let Some(d) = self.delimiter {
            if d != delimiter {
                return Err(Error::Config("delimiter already set".into()));
            }
            return Ok(());
        }
        let (tx, rx) = channel();
        self.keys.send(KeysJob::SetDelimiter(delimiter, tx)).map_err(|_| gone())?;
        rx.recv().map_err(|_| gone())??;
        self.delimiter = Some(delimiter);
        Ok(())
    }

    /// One merge: the pipelined equivalent of [`Index::ingest_batch`]. The
    /// keys are committed when this returns; the tree rows follow in the
    /// background and are committed by the next call or by [`Workers::join`].
    ///
    /// `cursor` and `scanned_total` are persisted in the same transaction,
    /// where `slot` says: the full scan's state, a subtree's record, or
    /// nowhere.
    pub fn merge(
        &mut self,
        mut batch: KeyBatch,
        generation: i64,
        slot: &ScanSlot,
        cursor: &str,
        scanned_total: u64,
    ) -> Result<IngestStats> {
        let delimiter = self
            .delimiter
            .ok_or_else(|| Error::Config("delimiter not set before merge".into()))?;
        let t0 = Instant::now();
        batch.normalise();
        let batch = Arc::new(batch);
        let seq = self.seq + 1;

        // phase 1: keys, committed with the cursor
        let (tx, rx) = channel();
        self.keys
            .send(KeysJob::Merge {
                batch: Arc::clone(&batch),
                generation,
                slot: slot.clone(),
                cursor: cursor.to_string(),
                scanned: scanned_total,
                seq,
                key_count: self.key_count,
                reply: tx,
            })
            .map_err(|_| gone())?;
        let (new_idx, stats) = rx.recv().map_err(|_| gone())??;
        self.key_count += stats.new as i64;
        self.seq = seq;
        let t_keys = t0.elapsed();

        // phase 2: deltas, while the shards may still work on the previous merge
        let entries = ingest::build_entries(&batch, &new_idx, delimiter, self.shards.len(), 1);
        let t_build = t0.elapsed() - t_keys;

        // close the previous merge on the shards before opening this one
        self.finish_pending(delimiter)?;
        let t_wait = t0.elapsed() - t_keys - t_build;

        // phase 3: node rows, all shards at once, left open
        let mut replies = Vec::with_capacity(self.shards.len());
        for (s, e) in self.shards.iter().zip(entries) {
            let (tx, rx) = channel();
            s.send(ShardJob::Upsert {
                batch: Arc::clone(&batch),
                entries: e,
                reply: tx,
            })
            .map_err(|_| gone())?;
            replies.push(rx);
        }
        let n_keys = batch.len();
        let batch_mb = (batch.bytes() + batch.len() * 8) >> 20;
        self.pending = Some(Pending {
            seq,
            batch,
            replies,
            started: Instant::now(),
        });
        tracing::debug!(
            keys = n_keys,
            new = stats.new,
            keys_ms = t_keys.as_millis() as u64,
            build_ms = t_build.as_millis() as u64,
            wait_prev_ms = t_wait.as_millis() as u64,
            rss_mb = crate::sysinfo::rss_bytes().unwrap_or(0) >> 20,
            sqlite_mb = sqlite_memory_used() >> 20,
            batch_mb,
            "merge submitted"
        );
        Ok(stats)
    }

    /// Phase 4 for the merge in flight: collect parent deltas, route them,
    /// commit every shard.
    fn finish_pending(&mut self, delimiter: u8) -> Result<()> {
        let Some(pending) = self.pending.take() else { return Ok(()) };
        let mut deltas = Vec::new();
        for rx in pending.replies {
            deltas.extend(rx.recv().map_err(|_| gone())??);
        }
        let t_upsert = pending.started.elapsed();
        let routed = ingest::route_parents(&pending.batch, deltas, self.shards.len());
        let mut replies = Vec::with_capacity(self.shards.len());
        for (i, (s, u)) in self.shards.iter().zip(routed.per_shard).enumerate() {
            let (tx, rx) = channel();
            let root = if i == 0 { Some(routed.root) } else { None };
            s.send(ShardJob::Commit {
                batch: Arc::clone(&pending.batch),
                delimiter,
                updates: u,
                root,
                seq: pending.seq,
                reply: tx,
            })
            .map_err(|_| gone())?;
            replies.push(rx);
        }
        for rx in replies {
            rx.recv().map_err(|_| gone())??;
        }
        tracing::debug!(
            seq = pending.seq,
            upsert_ms = t_upsert.as_millis() as u64,
            parents_commit_ms = (pending.started.elapsed() - t_upsert).as_millis() as u64,
            "merge committed"
        );
        Ok(())
    }

    /// Finish the merge in flight, stop the threads and reassemble the index.
    pub fn join(mut self) -> Result<Index> {
        let finish = match self.delimiter {
            Some(d) => self.finish_pending(d),
            None => Ok(()),
        };
        let (ktx, krx) = channel();
        self.keys.send(KeysJob::Finish(ktx)).map_err(|_| gone())?;
        let mut receivers = Vec::with_capacity(self.shards.len());
        for s in &self.shards {
            let (tx, rx) = channel();
            s.send(ShardJob::Finish(tx)).map_err(|_| gone())?;
            receivers.push(rx);
        }
        let keys = krx.recv().map_err(|_| gone())?;
        let mut shards = Vec::with_capacity(receivers.len());
        for rx in receivers {
            shards.push(rx.recv().map_err(|_| gone())?);
        }
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
        finish?;
        Ok(Index {
            keys,
            shards,
            location: self.location.clone(),
            delimiter: self.delimiter,
            key_count: self.key_count,
            fts: self.fts,
        })
    }
}

/// Bytes SQLite currently holds in page caches and other allocations.
pub fn sqlite_memory_used() -> u64 {
    // SAFETY: a plain query of a global counter, no pointers involved.
    unsafe { rusqlite::ffi::sqlite3_memory_used().max(0) as u64 }
}

fn gone() -> Error {
    Error::Scan("index writer thread is gone".into())
}

fn keys_thread(conn: Connection, rx: Receiver<KeysJob>, fts: bool) {
    while let Ok(job) = rx.recv() {
        match job {
            KeysJob::SetDelimiter(d, reply) => {
                let _ = reply.send(set_meta(&conn, "delimiter", &i64::from(d)));
            }
            KeysJob::Merge {
                batch,
                generation,
                slot,
                cursor,
                scanned,
                seq,
                key_count,
                reply,
            } => {
                let r = super::with_tx(&conn, |tx| {
                    let (new_idx, stats) = ingest::insert_keys(tx, &batch, generation, fts)?;
                    set_meta(tx, "key_count", &(key_count + stats.new as i64))?;
                    match &slot {
                        ScanSlot::Global => {
                            set_meta(tx, "scan.cursor", &cursor)?;
                            set_meta(tx, "scan.scanned", &(scanned as i64))?;
                        }
                        ScanSlot::Subtree(prefix) => {
                            tx.execute(
                                "UPDATE scans SET run_cursors = ?1, run_scanned = ?2 WHERE prefix = ?3",
                                rusqlite::params![cursor, scanned as i64, prefix],
                            )?;
                        }
                        ScanSlot::None => {}
                    }
                    set_meta(tx, "merge_seq", &seq)?;
                    Ok((new_idx, stats))
                });
                let _ = reply.send(r);
            }
            KeysJob::Finish(reply) => {
                let _ = reply.send(conn);
                return;
            }
        }
    }
}

fn shard_thread(conn: Connection, rx: Receiver<ShardJob>) {
    let mut open = false;
    while let Ok(job) = rx.recv() {
        match job {
            ShardJob::Upsert { batch, entries, reply } => {
                let r = (|| {
                    conn.execute_batch("BEGIN")?;
                    open = true;
                    ingest::upsert_nodes(&conn, &batch, &entries)
                })();
                if r.is_err() && open {
                    let _ = conn.execute_batch("ROLLBACK");
                    open = false;
                }
                let _ = reply.send(r);
            }
            ShardJob::Commit {
                batch,
                delimiter,
                updates,
                root,
                seq,
                reply,
            } => {
                let r = (|| {
                    if !open {
                        return Err(Error::Scan("commit without an open merge".into()));
                    }
                    let t = Instant::now();
                    ingest::apply_parent_updates(&conn, &batch, delimiter, &updates)?;
                    if let Some(root) = root {
                        ingest::apply_root_update(&conn, &root)?;
                    }
                    set_meta(&conn, "merge_seq", &seq)?;
                    let t_parents = t.elapsed();
                    conn.execute_batch("COMMIT")?;
                    open = false;
                    tracing::trace!(
                        parents = updates.len(),
                        parents_ms = t_parents.as_millis() as u64,
                        commit_ms = (t.elapsed() - t_parents).as_millis() as u64,
                        "shard commit"
                    );
                    Ok(())
                })();
                if r.is_err() && open {
                    let _ = conn.execute_batch("ROLLBACK");
                    open = false;
                }
                let _ = reply.send(r);
            }
            ShardJob::Finish(reply) => {
                if open {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                let _ = reply.send(conn);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexOptions, Sort};

    #[test]
    fn pipelined_merges_match_serial_ingest() {
        let mk = |shards| {
            let mut idx = Index::open(Location::Memory, &IndexOptions { cache_mb: 16, shards }).unwrap();
            idx.set_delimiter(b':').unwrap();
            let g = idx.begin_generation(false, None).unwrap();
            (idx, g)
        };
        let keys: Vec<Vec<u8>> = (0..3000)
            .map(|i| match i % 3 {
                0 => format!("user:{}:profile", i / 3),
                1 => format!("user:{}:cart", i / 7),
                _ => format!("order:{i:032x}"),
            })
            .map(String::into_bytes)
            .collect();
        let batch = |slice: &[Vec<u8>]| slice.iter().map(Vec::as_slice).collect::<KeyBatch>();
        let distinct = |slice: &[Vec<u8>]| slice.iter().collect::<std::collections::BTreeSet<_>>().len() as u64;

        let (mut serial, g) = mk(1);
        serial.ingest(keys.clone(), g, 5, 3000).unwrap();

        let (idx, g) = mk(3);
        let mut w = idx.into_workers().unwrap();
        let s1 = w.merge(batch(&keys[..1000]), g, &ScanSlot::Global, "1", 1000).unwrap();
        let s2 = w.merge(batch(&keys[500..2000]), g, &ScanSlot::Global, "3", 2000).unwrap();
        let s3 = w.merge(batch(&keys[1500..]), g, &ScanSlot::Global, "5", 3000).unwrap();
        assert_eq!(s1.new, distinct(&keys[..1000]));
        assert_eq!(s1.new + s2.new + s3.new, distinct(&keys));
        assert_eq!(w.key_count(), serial.key_count());
        let parallel = w.join().unwrap();

        assert_eq!(parallel.key_count(), serial.key_count());
        assert_eq!(parallel.scan_state().unwrap().cursor, 5);
        assert_eq!(parallel.root().unwrap(), serial.root().unwrap());
        for prefix in [&b""[..], b"user:", b"order:", b"user:1:"] {
            let a = parallel.children(prefix, None, 200, Sort::Name).unwrap();
            let b = serial.children(prefix, None, 200, Sort::Name).unwrap();
            assert_eq!(a, b, "prefix {:?}", String::from_utf8_lossy(prefix));
        }
        assert_eq!(parallel.table_counts().unwrap(), serial.table_counts().unwrap());
        assert_eq!(parallel.meta_i64("merge_seq").unwrap(), Some(3));
    }
}
