//! Mutations (CONCEPT section 7, write-through; section 8, safety).
//!
//! Every write the browser performs is an [`Op`] applied to one key by
//! [`apply`], which learns the key's state afterwards in the same pipeline
//! (`EXISTS`, `TTL`) and reports what the index has to do about it: the
//! caller runs [`write_through`] on the index writer so the tree never
//! shows a key the user just deleted. Pattern deletes go through the index
//! as batched `UNLINK` calls in a background task with pause and cancel
//! ([`start_delete`]); the index is updated as each batch is confirmed.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use redis::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::capability::value_text;
use crate::commands::{self, write};
use crate::connection::Conn;
use crate::error::{Error, Result};
use crate::index::Index;
use crate::route::{Hop, Router};
use crate::search::Plan;

/// One mutation of one key.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Delete,
    Rename {
        to: Vec<u8>,
    },
    /// Seconds must be positive; a non-positive `EXPIRE` would delete the key.
    Expire {
        seconds: i64,
    },
    Persist,
    /// `keep_ttl` preserves the expiry when editing an existing string.
    SetString {
        value: Vec<u8>,
        keep_ttl: bool,
    },
    HashSet {
        field: Vec<u8>,
        value: Vec<u8>,
    },
    HashDel {
        field: Vec<u8>,
    },
    SetAdd {
        member: Vec<u8>,
    },
    SetRem {
        member: Vec<u8>,
    },
    ZAdd {
        member: Vec<u8>,
        score: f64,
    },
    ZRem {
        member: Vec<u8>,
    },
    ListPush {
        value: Vec<u8>,
        head: bool,
    },
    ListSet {
        index: i64,
        value: Vec<u8>,
    },
    /// Remove the item at an index: `LSET` to a sentinel, then `LREM`.
    ListRem {
        index: i64,
    },
    StreamAdd {
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    },
    StreamDel {
        id: String,
    },
}

impl Op {
    /// The server command this runs, for confirmations and logs.
    pub fn command(&self) -> &'static str {
        match self {
            Op::Delete => "UNLINK",
            Op::Rename { .. } => "RENAME",
            Op::Expire { .. } => "EXPIRE",
            Op::Persist => "PERSIST",
            Op::SetString { .. } => "SET",
            Op::HashSet { .. } => "HSET",
            Op::HashDel { .. } => "HDEL",
            Op::SetAdd { .. } => "SADD",
            Op::SetRem { .. } => "SREM",
            Op::ZAdd { .. } => "ZADD",
            Op::ZRem { .. } => "ZREM",
            Op::ListPush { head: true, .. } => "LPUSH",
            Op::ListPush { head: false, .. } => "RPUSH",
            Op::ListSet { .. } => "LSET",
            Op::ListRem { .. } => "LREM",
            Op::StreamAdd { .. } => "XADD",
            Op::StreamDel { .. } => "XDEL",
        }
    }

    /// Deletes data (as opposed to adding or changing it).
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Op::Delete | Op::HashDel { .. } | Op::SetRem { .. } | Op::ZRem { .. } | Op::ListRem { .. } | Op::StreamDel { .. }
        )
    }

    fn validate(&self) -> Result<()> {
        match self {
            Op::Expire { seconds } if *seconds <= 0 => Err(Error::Config("expiry must be positive; use persist to remove it".into())),
            Op::Rename { to } if to.is_empty() => Err(Error::Config("new name is empty".into())),
            Op::StreamAdd { fields } if fields.is_empty() => Err(Error::Config("a stream entry needs at least one field".into())),
            _ => Ok(()),
        }
    }
}

/// What one mutation did, and what the index must do about it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// Names that no longer exist on the server.
    pub removed: Vec<Vec<u8>>,
    /// Names that exist on the server after the operation (upserted).
    pub added: Vec<Vec<u8>>,
    /// Expiry in seconds of the surviving key, `None` for none.
    pub ttl: Option<i64>,
    /// The server's reply to the mutation itself, as text.
    pub reply: String,
}

impl Outcome {
    /// The key survives (under its old or new name).
    pub fn exists(&self) -> bool {
        !self.added.is_empty()
    }
}

/// Apply one mutation and learn the key's state afterwards, in one
/// pipeline. `use_unlink` is what the capability layer says about `UNLINK`.
pub async fn apply(conn: &mut Conn, key: &[u8], op: &Op, use_unlink: bool) -> Result<Outcome> {
    op.validate()?;
    if let Op::Delete = op {
        let cmd = if use_unlink { write::unlink(&[key]) } else { write::del(&[key]) };
        let v = conn.raw(&cmd).await?;
        return Ok(Outcome {
            removed: vec![key.to_vec()],
            added: Vec::new(),
            ttl: None,
            reply: value_text(&v),
        });
    }
    let mut pipe = commands::pipeline();
    let mut mutations = 1usize;
    match op {
        Op::Delete => unreachable!(),
        Op::Rename { to } => {
            pipe.add_command(write::rename(key, to));
        }
        Op::Expire { seconds } => {
            pipe.add_command(write::expire(key, *seconds));
        }
        Op::Persist => {
            pipe.add_command(write::persist(key));
        }
        Op::SetString { value, keep_ttl } => {
            pipe.add_command(write::set(key, value, *keep_ttl));
        }
        Op::HashSet { field, value } => {
            pipe.add_command(write::hset(key, field, value));
        }
        Op::HashDel { field } => {
            pipe.add_command(write::hdel(key, field));
        }
        Op::SetAdd { member } => {
            pipe.add_command(write::sadd(key, member));
        }
        Op::SetRem { member } => {
            pipe.add_command(write::srem(key, member));
        }
        Op::ZAdd { member, score } => {
            pipe.add_command(write::zadd(key, *score, member));
        }
        Op::ZRem { member } => {
            pipe.add_command(write::zrem(key, member));
        }
        Op::ListPush { value, head: true } => {
            pipe.add_command(write::lpush(key, value));
        }
        Op::ListPush { value, head: false } => {
            pipe.add_command(write::rpush(key, value));
        }
        Op::ListSet { index, value } => {
            pipe.add_command(write::lset(key, *index, value));
        }
        Op::ListRem { index } => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let sentinel = format!("__keyfold_removed_{}_{nanos}__", std::process::id());
            pipe.add_command(write::lset(key, *index, sentinel.as_bytes()));
            pipe.add_command(write::lrem(key, 1, sentinel.as_bytes()));
            mutations = 2;
        }
        Op::StreamAdd { fields } => {
            pipe.add_command(write::xadd(key, fields));
        }
        Op::StreamDel { id } => {
            pipe.add_command(write::xdel(key, id));
        }
    }
    let after: &[u8] = match op {
        Op::Rename { to } => to,
        _ => key,
    };
    pipe.add_command(commands::exists(&[after]));
    pipe.add_command(commands::ttl(after));
    let replies = conn.pipeline(&pipe).await?;
    let mut reply = String::new();
    for r in replies.iter().take(mutations) {
        match r {
            Ok(v) => {
                if reply.is_empty() {
                    reply = value_text(v);
                }
            }
            Err(e) => return Err(e.clone_shallow()),
        }
    }
    let exists = matches!(replies.get(mutations), Some(Ok(Value::Int(n))) if *n > 0);
    let ttl = match replies.get(mutations + 1) {
        Some(Ok(Value::Int(n))) if *n >= 0 => Some(*n),
        _ => None,
    };
    let mut outcome = Outcome {
        removed: Vec::new(),
        added: Vec::new(),
        ttl: if exists { ttl } else { None },
        reply,
    };
    if let Op::Rename { .. } = op {
        outcome.removed.push(key.to_vec());
    }
    if exists {
        outcome.added.push(after.to_vec());
    } else {
        outcome.removed.push(after.to_vec());
    }
    Ok(outcome)
}

impl Error {
    /// Errors are not `Clone`; pipelines hand back borrowed ones.
    fn clone_shallow(&self) -> Error {
        match self {
            Error::Denied { command } => Error::Denied { command: command.clone() },
            Error::Server { code, detail } => Error::Server {
                code: code.clone(),
                detail: detail.clone(),
            },
            other => Error::Scan(other.to_string()),
        }
    }
}

/// What [`write_through`] changed in the index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexChange {
    pub added: u64,
    pub removed: u64,
}

impl IndexChange {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// Bring the index in line with an [`Outcome`]: removed names go, surviving
/// names are upserted with their expiry. An index that was never scanned
/// (no delimiter yet) has nothing to keep in line.
pub fn write_through(index: &mut Index, outcome: &Outcome) -> Result<IndexChange> {
    if index.delimiter().is_none() {
        return Ok(IndexChange::default());
    }
    let mut change = IndexChange::default();
    if !outcome.removed.is_empty() {
        change.removed = index.remove(&outcome.removed)?;
    }
    if !outcome.added.is_empty() {
        let generation = index.scan_state()?.generation;
        let stats = index.add(outcome.added.clone(), generation)?;
        change.added = stats.new;
        for name in &outcome.added {
            index.set_key_meta(name, outcome.ttl, None)?;
        }
    }
    Ok(change)
}

/// `FLUSHDB` (or `FLUSHALL`) on one node. The caller clears the index.
/// A cluster is flushed on every primary through [`Router::flush`].
pub async fn flush(conn: &mut Conn, all: bool) -> Result<()> {
    let cmd = if all { write::flushall() } else { write::flushdb() };
    conn.raw(&cmd).await?;
    Ok(())
}

// ---- pattern delete ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DeleteConfig {
    /// Keys per `UNLINK` call.
    pub batch: usize,
    /// Pause between calls (the polite profile's gap on prod).
    pub gap: Duration,
    pub use_unlink: bool,
}

impl Default for DeleteConfig {
    fn default() -> Self {
        DeleteConfig {
            batch: 500,
            gap: Duration::ZERO,
            use_unlink: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeletePhase {
    #[default]
    Counting,
    Deleting,
    Paused,
    Complete,
    Cancelled,
    Failed,
}

/// Live progress of a pattern delete, published after every call.
#[derive(Debug, Clone, Default)]
pub struct DeleteProgress {
    pub phase: DeletePhase,
    /// Exact match count in the index when the delete started.
    pub total: Option<i64>,
    /// Names taken from the index so far.
    pub matched: u64,
    /// Keys the server reported deleted.
    pub deleted: u64,
    /// Rows removed from the index.
    pub removed: u64,
    pub calls: u64,
    pub elapsed: Duration,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Run,
    Pause,
    Cancel,
}

#[derive(Debug)]
pub struct DeleteOutcome {
    pub index: Index,
    pub router: Router,
    pub progress: DeleteProgress,
}

/// Handle to a running pattern delete.
pub struct DeleteHandle {
    control: watch::Sender<Control>,
    progress: watch::Receiver<DeleteProgress>,
    task: JoinHandle<Result<DeleteOutcome>>,
}

impl DeleteHandle {
    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }

    pub fn resume(&self) {
        let _ = self.control.send(Control::Run);
    }

    /// Stop after the current call; what was deleted stays deleted and the
    /// index is in line with it.
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    pub fn progress(&self) -> watch::Receiver<DeleteProgress> {
        self.progress.clone()
    }

    pub async fn wait(self) -> Result<DeleteOutcome> {
        match self.task.await {
            Ok(r) => r,
            Err(e) => Err(Error::Scan(format!("delete task panicked: {e}"))),
        }
    }
}

/// Delete every key the index matches for `plan`, in batches, on its own
/// connection, owning the index writer for the duration. The index is
/// read in windows and each window is removed from it once the server
/// confirmed the call, so a cancel leaves both in step.
pub fn start_delete(router: Router, index: Index, plan: Plan, config: DeleteConfig) -> DeleteHandle {
    let (control_tx, control_rx) = watch::channel(Control::Run);
    let (progress_tx, progress_rx) = watch::channel(DeleteProgress::default());
    let task = tokio::spawn(run_delete(router, index, plan, config, control_rx, progress_tx));
    DeleteHandle {
        control: control_tx,
        progress: progress_rx,
        task,
    }
}

async fn run_delete(
    mut router: Router,
    index: Index,
    plan: Plan,
    config: DeleteConfig,
    mut control: watch::Receiver<Control>,
    progress_tx: watch::Sender<DeleteProgress>,
) -> Result<DeleteOutcome> {
    let started = Instant::now();
    let mut progress = DeleteProgress::default();
    let _ = progress_tx.send_replace(progress.clone());
    let batch = config.batch.clamp(1, 1000);

    // The exact count first, so the UI can show "1,204 of 5,000".
    let plan_count = plan.clone();
    let (mut index, total) = tokio::task::spawn_blocking(move || {
        let never = AtomicBool::new(false);
        let total = index.search_count(&plan_count, &never);
        (index, total)
    })
    .await
    .map_err(|e| Error::Scan(format!("delete count task panicked: {e}")))?;
    progress.total = total?;
    progress.phase = DeletePhase::Deleting;
    progress.elapsed = started.elapsed();
    let _ = progress_tx.send_replace(progress.clone());

    let mut after: Option<Vec<u8>> = None;
    let mut outcome_phase = DeletePhase::Complete;
    let mut failure: Option<Error> = None;
    loop {
        // Honour pause and cancel between calls.
        let mut cancelled = false;
        loop {
            let current = *control.borrow_and_update();
            match current {
                Control::Run => break,
                Control::Cancel => {
                    cancelled = true;
                    break;
                }
                Control::Pause => {
                    progress.phase = DeletePhase::Paused;
                    progress.elapsed = started.elapsed();
                    let _ = progress_tx.send_replace(progress.clone());
                    if control.changed().await.is_err() {
                        cancelled = true;
                        break;
                    }
                    progress.phase = DeletePhase::Deleting;
                }
            }
        }
        if cancelled {
            outcome_phase = DeletePhase::Cancelled;
            break;
        }

        // Up to `batch` names from the index, in windows of at most 200.
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(batch);
        let mut exhausted = false;
        while names.len() < batch && !exhausted {
            let plan_w = plan.clone();
            let after_w = after.clone();
            let want = batch - names.len();
            let (idx, page) = tokio::task::spawn_blocking(move || {
                let never = AtomicBool::new(false);
                let page = index.search(&plan_w, after_w.as_deref(), want, &never);
                (index, page)
            })
            .await
            .map_err(|e| Error::Scan(format!("delete window task panicked: {e}")))?;
            index = idx;
            match page {
                Ok(p) => {
                    exhausted = p.done || p.rows.is_empty();
                    for r in p.rows {
                        after = Some(r.name.clone());
                        names.push(r.name);
                    }
                }
                Err(e) => {
                    failure = Some(e);
                    exhausted = true;
                }
            }
        }
        if failure.is_some() {
            break;
        }
        if names.is_empty() {
            break;
        }
        progress.matched += names.len() as u64;

        // The server first; the index only once the server confirmed.
        let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        match delete_batch(&mut router, &refs, config.use_unlink).await {
            Ok((deleted, calls)) => {
                progress.deleted += deleted;
                progress.calls += calls;
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
        let (idx, removed) = tokio::task::spawn_blocking(move || {
            let removed = index.remove(&names);
            (index, removed)
        })
        .await
        .map_err(|e| Error::Scan(format!("delete remove task panicked: {e}")))?;
        index = idx;
        match removed {
            Ok(n) => progress.removed += n,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
        progress.elapsed = started.elapsed();
        let _ = progress_tx.send_replace(progress.clone());
        if exhausted {
            break;
        }
        if !config.gap.is_zero() {
            tokio::time::sleep(config.gap).await;
        }
    }

    progress.elapsed = started.elapsed();
    if let Some(e) = &failure {
        progress.phase = DeletePhase::Failed;
        progress.error = Some(e.to_string());
    } else {
        progress.phase = outcome_phase;
    }
    let _ = progress_tx.send_replace(progress.clone());
    Ok(DeleteOutcome { index, router, progress })
}

/// Delete one batch: a single multi-key `UNLINK` on a standalone server;
/// on a cluster one pipeline of single-key `UNLINK` calls per owning node
/// (multi-key commands across slots are refused with `CROSSSLOT`).
/// Returns the keys the server reported deleted and the round trips used.
async fn delete_batch(router: &mut Router, names: &[&[u8]], use_unlink: bool) -> Result<(u64, u64)> {
    let name = if use_unlink { "UNLINK" } else { "DEL" };
    let one = |keys: &[&[u8]]| if use_unlink { write::unlink(keys) } else { write::del(keys) };
    if !router.is_cluster() {
        let seed = router.seed().clone();
        let cmd = one(names);
        let v = crate::routed!(router, Hop::node(&seed), |c| c.raw(&cmd).await)?;
        return match v {
            Value::Int(n) => Ok((n.max(0) as u64, 1)),
            other => Err(Error::reply(name, format!("{other:?}"))),
        };
    }
    let mut deleted = 0u64;
    let mut calls = 0u64;
    for (addr, idxs) in router.group_by_node(names) {
        let mut pipe = commands::pipeline();
        for &i in &idxs {
            pipe.add_command(one(&[names[i]]));
        }
        let replies = crate::routed!(router, Hop::node(&addr), |c| c.pipeline(&pipe).await)?;
        calls += 1;
        for r in replies {
            match r {
                Ok(Value::Int(n)) => deleted += n.max(0) as u64,
                Ok(other) => return Err(Error::reply(name, format!("{other:?}"))),
                Err(e) if e.redirect().is_some() => {
                    // the slot moved between batches: refresh and let the
                    // next batch route correctly; this key is retried alone
                    router.refresh().await?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok((deleted, calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_name_their_command_and_destructiveness() {
        assert_eq!(Op::Delete.command(), "UNLINK");
        assert!(Op::Delete.is_destructive());
        assert_eq!(Op::ListPush { value: vec![], head: true }.command(), "LPUSH");
        assert_eq!(
            Op::ListPush {
                value: vec![],
                head: false
            }
            .command(),
            "RPUSH"
        );
        assert!(
            !Op::HashSet {
                field: vec![],
                value: vec![]
            }
            .is_destructive()
        );
        assert!(Op::ListRem { index: 0 }.is_destructive());
    }

    #[test]
    fn a_non_positive_expiry_is_refused() {
        assert!(Op::Expire { seconds: 0 }.validate().is_err());
        assert!(Op::Expire { seconds: -5 }.validate().is_err());
        assert!(Op::Expire { seconds: 1 }.validate().is_ok());
        assert!(Op::Rename { to: vec![] }.validate().is_err());
        assert!(Op::StreamAdd { fields: vec![] }.validate().is_err());
    }

    #[test]
    fn write_through_is_a_no_op_on_an_unscanned_index() {
        use crate::index::{Index, IndexOptions, Location};
        let mut index = Index::open(Location::Memory, &IndexOptions { cache_mb: 4, shards: 1 }).unwrap();
        let outcome = Outcome {
            removed: vec![b"a".to_vec()],
            added: vec![b"b".to_vec()],
            ttl: Some(10),
            reply: "OK".into(),
        };
        assert_eq!(write_through(&mut index, &outcome).unwrap(), IndexChange::default());
        index.set_delimiter(b':').unwrap();
        let change = write_through(&mut index, &outcome).unwrap();
        assert_eq!(change, IndexChange { added: 1, removed: 0 });
        assert_eq!(index.key_count(), 1);
        let change = write_through(
            &mut index,
            &Outcome {
                removed: vec![b"b".to_vec()],
                ..Outcome::default()
            },
        )
        .unwrap();
        assert_eq!(change, IndexChange { added: 0, removed: 1 });
        assert_eq!(index.key_count(), 0);
    }
}
