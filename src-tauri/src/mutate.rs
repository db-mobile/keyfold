//! Write commands (CONCEPT section 7, write-through; section 8, safety):
//! single-key mutations that update the index in the same action, pattern
//! deletes through the index as batched `UNLINK` with pause and cancel,
//! and `FLUSHDB`/`FLUSHALL` behind a typed confirmation. The backend
//! enforces the confirmations: a prod-tagged connection refuses an
//! unconfirmed write, a flush refuses a wrong name.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use keyfold_core::mutate::{self, DeleteConfig, DeletePhase, DeleteProgress, Op};
use keyfold_core::route::Hop;
use keyfold_core::search::{self, Plan, Query};
use keyfold_core::{Router, ScanProfile, Tag, routed};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::Sessions;
use crate::session::Session;
use crate::wire::{KeyArg, bytes_from, display_and_raw};

/// One mutation of one key, as the webview sends it. Values are text; the
/// editors do not write binary values. A field or member that already
/// exists is addressed by its exact bytes (`field_raw`, `member_raw`, hex)
/// when it is not UTF-8.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", tag = "op")]
pub enum OpDto {
    Delete,
    Rename {
        to: String,
    },
    Expire {
        seconds: i64,
    },
    Persist,
    SetString {
        value: String,
        keep_ttl: bool,
    },
    HashSet {
        field: String,
        #[serde(default)]
        field_raw: Option<String>,
        value: String,
    },
    HashDel {
        field: String,
        #[serde(default)]
        field_raw: Option<String>,
    },
    SetAdd {
        member: String,
    },
    SetRem {
        member: String,
        #[serde(default)]
        member_raw: Option<String>,
    },
    ZAdd {
        member: String,
        #[serde(default)]
        member_raw: Option<String>,
        score: f64,
    },
    ZRem {
        member: String,
        #[serde(default)]
        member_raw: Option<String>,
    },
    ListPush {
        value: String,
        head: bool,
    },
    ListSet {
        index: i64,
        value: String,
    },
    ListRem {
        index: i64,
    },
    StreamAdd {
        fields: Vec<(String, String)>,
    },
    StreamDel {
        id: String,
    },
}

impl TryFrom<OpDto> for Op {
    type Error = String;

    fn try_from(d: OpDto) -> Result<Op, String> {
        let b = |s: String| s.into_bytes();
        let exact = |text: String, raw: Option<String>| bytes_from(&text, raw.as_deref());
        Ok(match d {
            OpDto::Delete => Op::Delete,
            OpDto::Rename { to } => Op::Rename { to: b(to) },
            OpDto::Expire { seconds } => Op::Expire { seconds },
            OpDto::Persist => Op::Persist,
            OpDto::SetString { value, keep_ttl } => Op::SetString { value: b(value), keep_ttl },
            OpDto::HashSet { field, field_raw, value } => Op::HashSet {
                field: exact(field, field_raw)?,
                value: b(value),
            },
            OpDto::HashDel { field, field_raw } => Op::HashDel {
                field: exact(field, field_raw)?,
            },
            OpDto::SetAdd { member } => Op::SetAdd { member: b(member) },
            OpDto::SetRem { member, member_raw } => Op::SetRem {
                member: exact(member, member_raw)?,
            },
            OpDto::ZAdd { member, member_raw, score } => Op::ZAdd {
                member: exact(member, member_raw)?,
                score,
            },
            OpDto::ZRem { member, member_raw } => Op::ZRem {
                member: exact(member, member_raw)?,
            },
            OpDto::ListPush { value, head } => Op::ListPush { value: b(value), head },
            OpDto::ListSet { index, value } => Op::ListSet { index, value: b(value) },
            OpDto::ListRem { index } => Op::ListRem { index },
            OpDto::StreamAdd { fields } => Op::StreamAdd {
                fields: fields.into_iter().map(|(f, v)| (b(f), b(v))).collect(),
            },
            OpDto::StreamDel { id } => Op::StreamDel { id },
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutateResultDto {
    pub command: &'static str,
    /// The key exists after the operation (under its new name for a rename).
    pub exists: bool,
    pub ttl: Option<i64>,
    pub reply: String,
    pub index_added: u64,
    pub index_removed: u64,
    /// A scan owns the index; the index update is applied when it ends.
    pub queued: bool,
    pub took_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletePreviewDto {
    pub mode: &'static str,
    pub count: i64,
    pub sample: Vec<String>,
    pub took_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteProgressDto {
    pub session: u32,
    pub query: String,
    /// `counting`, `deleting`, `paused`, `complete`, `cancelled`, `failed`
    pub phase: &'static str,
    pub total: Option<i64>,
    pub matched: u64,
    pub deleted: u64,
    pub removed: u64,
    pub calls: u64,
    pub elapsed_ms: f64,
    pub error: Option<String>,
}

/// Emitted after every index change made behind the windows' back, so the
/// webview refetches what it shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexChangedDto {
    pub session: u32,
    pub reason: &'static str,
    pub added: u64,
    pub removed: u64,
}

fn phase_name(p: DeletePhase) -> &'static str {
    match p {
        DeletePhase::Counting => "counting",
        DeletePhase::Deleting => "deleting",
        DeletePhase::Paused => "paused",
        DeletePhase::Complete => "complete",
        DeletePhase::Cancelled => "cancelled",
        DeletePhase::Failed => "failed",
    }
}

fn progress_dto(session: u32, query: &str, p: &DeleteProgress) -> DeleteProgressDto {
    DeleteProgressDto {
        session,
        query: query.to_string(),
        phase: phase_name(p.phase),
        total: p.total,
        matched: p.matched,
        deleted: p.deleted,
        removed: p.removed,
        calls: p.calls,
        elapsed_ms: p.elapsed.as_secs_f64() * 1000.0,
        error: p.error.clone(),
    }
}

pub(crate) fn emit_index_changed(app: &AppHandle, session: u32, reason: &'static str, added: u64, removed: u64) {
    let _ = app.emit(
        "index:changed",
        &IndexChangedDto {
            session,
            reason,
            added,
            removed,
        },
    );
}

fn require_confirmed(session: &Session, confirmed: bool) -> Result<(), String> {
    if session.profile.tag == Tag::Prod && !confirmed {
        return Err("this connection is tagged prod: every write needs a confirmation".into());
    }
    Ok(())
}

/// One mutation, written through to the index in the same action.
#[tauri::command]
pub async fn mutate(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    id: u32,
    key: KeyArg,
    op: OpDto,
    confirmed: bool,
) -> Result<MutateResultDto, String> {
    let session = sessions.get(id).await?;
    require_confirmed(&session, confirmed)?;
    let op: Op = op.try_into()?;
    let command = op.command();
    let shown = key.text.clone();
    let key = key.bytes()?;
    let t = Instant::now();
    let outcome = {
        let k = &key[..];
        let use_unlink = session.caps.has("unlink");
        let mut router = session.router.lock().await;
        if let Op::Rename { to } = &op
            && router.is_cluster()
            && keyfold_core::cluster::key_slot(k) != keyfold_core::cluster::key_slot(to)
        {
            return Err(format!(
                "on a cluster a key can only be renamed within its hash slot (slot {} to {}); use a {{hashtag}} shared by both names",
                keyfold_core::cluster::key_slot(k),
                keyfold_core::cluster::key_slot(to)
            ));
        }
        routed!(router, Hop::key(k), |c| mutate::apply(c, k, &op, use_unlink).await).map_err(|e| e.to_string())?
    };
    // a refused UNLINK was done again as DEL, and is remembered as denied
    let command = if matches!(op, Op::Delete) && !session.caps.has("unlink") {
        "DEL"
    } else {
        command
    };
    let exists = outcome.exists();
    let ttl = outcome.ttl;
    let reply = outcome.reply.clone();
    let (change, queued) = session.write_through(outcome).await?;
    if !change.is_empty() {
        emit_index_changed(&app, id, "mutate", change.added, change.removed);
    }
    tracing::info!(id, key = %shown, command, exists, ?ttl, reply, added = change.added, removed = change.removed, queued, took = ?t.elapsed(), "mutate");
    Ok(MutateResultDto {
        command,
        exists,
        ttl,
        reply,
        index_added: change.added,
        index_removed: change.removed,
        queued,
        took_ms: t.elapsed().as_secs_f64() * 1000.0,
    })
}

/// What a pattern delete matches: the typed `query`, or, when the delete
/// was started on a tree node, every key under exactly that `prefix`
/// (`query` is then only the text shown and echoed in the progress).
fn delete_plan(query: &str, prefix: Option<&KeyArg>) -> Result<Plan, String> {
    let q = match prefix {
        Some(p) => {
            let bytes = p.bytes()?;
            if bytes.is_empty() {
                return Err("an empty prefix would match every key".into());
            }
            Query::prefix(&bytes)
        }
        None => search::parse(query),
    };
    Plan::new(q).map_err(|e| e.to_string())
}

/// Exact count and a 20-key sample for the confirmation (CONCEPT 8).
#[tauri::command]
pub async fn delete_preview(
    sessions: State<'_, Sessions>,
    id: u32,
    query: String,
    prefix: Option<KeyArg>,
) -> Result<DeletePreviewDto, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let plan = delete_plan(&query, prefix.as_ref())?;
    let t = Instant::now();
    tokio::task::spawn_blocking(move || -> Result<DeletePreviewDto, String> {
        let never = AtomicBool::new(false);
        let mut reader = session.search_reader.lock().map_err(|_| "search reader poisoned")?;
        reader.reload_meta().map_err(|e| e.to_string())?;
        let count = reader.search_count(&plan, &never).map_err(|e| e.to_string())?.unwrap_or(0);
        let page = reader.search(&plan, None, 20, &never).map_err(|e| e.to_string())?;
        Ok(DeletePreviewDto {
            mode: plan.query.mode.name(),
            count,
            sample: page.rows.iter().map(|r| display_and_raw(&r.name).0).collect(),
            took_ms: t.elapsed().as_secs_f64() * 1000.0,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Start deleting every index match of `query` in the background, as
/// batched `UNLINK` calls on its own connection, owning the index writer;
/// progress arrives as `delete:progress`. Always needs a confirmation.
#[tauri::command]
pub async fn delete_start(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    id: u32,
    query: String,
    prefix: Option<KeyArg>,
    confirmed: bool,
) -> Result<(), String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    if !confirmed {
        return Err("a pattern delete needs a confirmation".into());
    }
    let plan = delete_plan(&query, prefix.as_ref())?;
    if session.is_scanning().await {
        return Err("wait for the scan to finish".into());
    }
    let mut slot = session.delete.lock().await;
    if slot.is_some() {
        return Err("a delete is already running".into());
    }
    let Some(index) = session.take_writer().await else {
        return Err("index is busy".into());
    };
    let router = match session.manager.connect().await {
        Ok(c) => Router::new(session.manager.clone(), c, &session.caps).await,
        Err(e) => {
            session.return_writer(index).await;
            return Err(e.to_string());
        }
    };
    let config = DeleteConfig {
        gap: if session.profile.tag == Tag::Prod {
            ScanProfile::Polite.gap()
        } else {
            Duration::ZERO
        },
        use_unlink: session.caps.has("unlink"),
        ..DeleteConfig::default()
    };
    let handle = mutate::start_delete(router, index, plan, config);
    let mut progress = handle.progress();
    *slot = Some(handle);
    drop(slot);
    tracing::info!(id, query, by_prefix = prefix.is_some(), "delete_start");

    let s = Arc::clone(&session);
    tokio::spawn(async move {
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        loop {
            if progress.changed().await.is_err() {
                break;
            }
            let mut p = progress.borrow().clone();
            let terminal = matches!(p.phase, DeletePhase::Complete | DeletePhase::Cancelled | DeletePhase::Failed);
            if !terminal {
                let since = last_emit.elapsed();
                if since < Duration::from_millis(250) {
                    tokio::time::sleep(Duration::from_millis(250) - since).await;
                    p = progress.borrow().clone();
                }
            }
            let _ = app.emit("delete:progress", &progress_dto(s.id, &query, &p));
            last_emit = Instant::now();
            if matches!(p.phase, DeletePhase::Complete | DeletePhase::Cancelled | DeletePhase::Failed) {
                break;
            }
        }
        let handle = s.delete.lock().await.take();
        let mut final_progress = None;
        if let Some(h) = handle {
            match h.wait().await {
                Ok(outcome) => {
                    s.return_writer(outcome.index).await;
                    final_progress = Some(outcome.progress);
                }
                Err(e) => {
                    tracing::warn!("delete ended with error: {e}");
                    s.reopen_writer().await;
                    let mut p = progress.borrow().clone();
                    p.phase = DeletePhase::Failed;
                    p.error = Some(e.to_string());
                    final_progress = Some(p);
                }
            }
        }
        s.invalidate_views();
        if let Some(p) = final_progress {
            tracing::info!(id = s.id, query, phase = ?p.phase, deleted = p.deleted, removed = p.removed, calls = p.calls, took = ?p.elapsed, "delete_end");
            let _ = app.emit("delete:progress", &progress_dto(s.id, &query, &p));
            emit_index_changed(&app, s.id, "delete", 0, p.removed);
        }
    });
    Ok(())
}

#[tauri::command]
pub async fn delete_pause(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    if let Some(h) = sessions.get(id).await?.delete.lock().await.as_ref() {
        h.pause();
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_resume(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    if let Some(h) = sessions.get(id).await?.delete.lock().await.as_ref() {
        h.resume();
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_cancel(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    if let Some(h) = sessions.get(id).await?.delete.lock().await.as_ref() {
        h.cancel();
    }
    Ok(())
}

/// `FLUSHDB` (or `FLUSHALL`) after the user typed the connection's name;
/// the index is cleared and, when the connection indexes automatically,
/// rescanned (one `SCAN` call on an empty database).
#[tauri::command]
pub async fn flush(app: AppHandle, sessions: State<'_, Sessions>, id: u32, all: bool, confirm: String) -> Result<(), String> {
    let session = sessions.get(id).await?;
    if confirm.trim() != session.profile.name {
        return Err(format!("type the connection name {:?} to confirm", session.profile.name));
    }
    if session.is_scanning().await {
        return Err("wait for the scan to finish".into());
    }
    if session.delete.lock().await.is_some() {
        return Err("a delete is running".into());
    }
    let Some(mut index) = session.take_writer().await else {
        return Err("index is busy".into());
    };
    let t = Instant::now();
    let result = session.router.lock().await.flush(all).await;
    if let Err(e) = result {
        session.return_writer(index).await;
        return Err(e.to_string());
    }
    let (index, cleared) = tokio::task::spawn_blocking(move || {
        let r = index.clear();
        (index, r)
    })
    .await
    .map_err(|e| e.to_string())?;
    let keys_before = index.key_count();
    session.return_writer(index).await;
    cleared.map_err(|e| e.to_string())?;
    session.invalidate_views();
    tracing::warn!(id, all, took = ?t.elapsed(), "flush");
    emit_index_changed(&app, id, "flush", 0, keys_before as u64);
    session.start_default(app).await?;
    Ok(())
}
