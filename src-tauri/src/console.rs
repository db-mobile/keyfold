//! The command console (CONCEPT 8): one typed line in, a bounded number
//! of reply lines out. The console has a connection of its own, opened on
//! first use. What may run, and what needs a confirmation first, is
//! decided here and not in the webview: a line that asks for one comes
//! back unsent, and only a second call that carries the confirmation
//! sends it. After a write the keys it named are read again and written
//! through to the index.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use keyfold_core::console::{self, Action, Confirm, Console, Effect, Limits};
use keyfold_core::{IndexMode, NodeAddr, Router, Tag, scan};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::Sessions;
use crate::mutate::emit_index_changed;
use crate::session::Session;

/// What the console wants confirmed before it sends a line.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleConfirmDto {
    /// `write` (a prod-tagged connection), `danger`, or `typed`.
    pub level: &'static str,
    pub text: String,
    /// The text to type, for `typed`: the connection's name.
    pub expect: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleReplyDto {
    /// `ok`, `error` (the server's or the connection's), `refused` (not
    /// sent, `lines` say why) or `confirm` (not sent yet).
    pub status: &'static str,
    pub lines: Vec<String>,
    /// The reply was longer than what `lines` show.
    pub truncated: bool,
    /// The node that answered.
    pub node: Option<String>,
    pub took_ms: f64,
    pub confirm: Option<ConsoleConfirmDto>,
    /// What the command meant for the index, in a few words.
    pub index_note: Option<String>,
}

impl ConsoleReplyDto {
    fn unsent(status: &'static str, lines: Vec<String>, confirm: Option<ConsoleConfirmDto>) -> ConsoleReplyDto {
        ConsoleReplyDto {
            status,
            lines,
            truncated: false,
            node: None,
            took_ms: 0.0,
            confirm,
            index_note: None,
        }
    }
}

/// `host:port` of a node the connection knows; anything else is refused,
/// so the console cannot be pointed at an arbitrary address.
async fn node_from(session: &Session, node: Option<String>) -> Result<Option<NodeAddr>, String> {
    let Some(text) = node.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let router = session.router.lock().await;
    scan::known_nodes(&session.caps, router.topology(), router.seed())
        .into_iter()
        .map(|(addr, _, _)| addr)
        .find(|a| a.to_string() == text)
        .map(Some)
        .ok_or_else(|| format!("{text} is not a node of this connection"))
}

/// Run one typed line. `confirmed` answers a confirmation asked for by an
/// earlier call with the same line: any text for `write` and `danger`,
/// the connection's name for `typed`.
#[tauri::command]
pub async fn console_run(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    id: u32,
    line: String,
    node: Option<String>,
    confirmed: Option<String>,
) -> Result<ConsoleReplyDto, String> {
    let session = sessions.get(id).await?;
    let args = match console::split_line(&line) {
        Ok(a) if a.is_empty() => return Ok(ConsoleReplyDto::unsent("ok", Vec::new(), None)),
        Ok(a) => a,
        Err(e) => return Ok(ConsoleReplyDto::unsent("refused", vec![plain(&e)], None)),
    };
    let node = node_from(&session, node).await?;
    let prod = session.profile.tag == Tag::Prod;

    let mut slot = session.console.lock().await;
    if slot.is_none() {
        let conn = session.manager.connect().await.map_err(|e| e.to_string())?;
        *slot = Some(Console::new(Router::new(session.manager.clone(), conn, &session.caps).await));
        tracing::info!(id, "console connection opened");
    }
    let con = slot.as_mut().ok_or("no console")?;
    let verdict = match con.inspect(&args, prod).await {
        Ok(v) => v,
        Err(e) => {
            // the console's connection may be gone: the next line opens a new one
            *slot = None;
            return Err(e.to_string());
        }
    };
    match &verdict.action {
        Action::Run => {}
        Action::Refuse(why) => {
            tracing::info!(id, command = %String::from_utf8_lossy(&args[0]), "console refused");
            return Ok(ConsoleReplyDto::unsent("refused", vec![why.clone()], None));
        }
        Action::Confirm(c) => {
            let (level, text, expect) = match c {
                Confirm::Write => (
                    "write",
                    format!(
                        "{} is tagged prod and this command is not known to be read-only.",
                        session.profile.name
                    ),
                    None,
                ),
                Confirm::Danger(t) => ("danger", t.clone(), None),
                Confirm::Typed(t) => ("typed", t.clone(), Some(session.profile.name.clone())),
            };
            let given = match (&expect, confirmed.as_deref()) {
                (Some(name), Some(typed)) => typed.trim() == name,
                (None, Some(_)) => true,
                (_, None) => false,
            };
            if !given {
                return Ok(ConsoleReplyDto::unsent(
                    "confirm",
                    Vec::new(),
                    Some(ConsoleConfirmDto { level, text, expect }),
                ));
            }
        }
    }

    let ran = match con.run(&args, node.as_ref(), &verdict).await {
        Ok(r) => r,
        // not sent: keys in different slots, for example
        Err(e) => return Ok(ConsoleReplyDto::unsent("refused", vec![plain(&e)], None)),
    };
    let (status, rendered) = match &ran.reply {
        Ok(v) => ("ok", console::render(v, &Limits::default())),
        Err(e) => (
            "error",
            console::Rendered {
                lines: vec![console::render_error(e)],
                truncated: false,
            },
        ),
    };
    if matches!(&ran.reply, Err(keyfold_core::Error::Client(_)) | Err(keyfold_core::Error::Io(_))) {
        *slot = None;
    }
    drop(slot);
    tracing::info!(id, command = %String::from_utf8_lossy(&args[0]), status, node = %ran.node, lines = rendered.lines.len(), effect = ?effect_name(&ran.effect), took = ?ran.took, "console_run");

    let index_note = follow(&app, &session, &ran.effect).await;
    Ok(ConsoleReplyDto {
        status,
        lines: rendered.lines,
        truncated: rendered.truncated,
        node: Some(ran.node.to_string()),
        took_ms: ran.took.as_secs_f64() * 1000.0,
        confirm: None,
        index_note,
    })
}

/// Why a line was not sent, without the error kind in front of it.
fn plain(e: &keyfold_core::Error) -> String {
    match e {
        keyfold_core::Error::Config(text) => text.clone(),
        other => other.to_string(),
    }
}

fn effect_name(e: &Effect) -> &'static str {
    match e {
        Effect::None => "none",
        Effect::Keys(_) => "keys",
        Effect::Flushed => "flushed",
        Effect::Unknown => "unknown",
    }
}

/// Bring the index in line with what a console command did.
async fn follow(app: &AppHandle, session: &Arc<Session>, effect: &Effect) -> Option<String> {
    if session.index_mode == IndexMode::None {
        return None;
    }
    match effect {
        Effect::None => None,
        Effect::Keys(keys) => {
            let outcomes = {
                let mut router = session.router.lock().await;
                console::reconcile(&mut router, keys).await
            };
            let outcomes = match outcomes {
                Ok(o) => o,
                Err(e) => {
                    session.index_stale.store(true, Ordering::Relaxed);
                    return Some(format!("the keys could not be read again ({e}); the index may be stale"));
                }
            };
            match session.write_through_many(outcomes).await {
                Ok((change, queued)) => {
                    if !change.is_empty() {
                        emit_index_changed(app, session.id, "mutate", change.added, change.removed);
                    }
                    Some(if queued {
                        "index update queued until the scan ends".to_string()
                    } else if change.is_empty() {
                        "index unchanged".to_string()
                    } else {
                        format!("index: {} added, {} removed", change.added, change.removed)
                    })
                }
                Err(e) => {
                    session.index_stale.store(true, Ordering::Relaxed);
                    Some(format!("the index could not follow ({e}); it may be stale"))
                }
            }
        }
        Effect::Flushed => {
            // as the flush command does: empty the index and start over
            let Some(mut index) = session.take_writer().await else {
                session.index_stale.store(true, Ordering::Relaxed);
                return Some("the index is busy and still shows the keys; rescan when the scan has ended".to_string());
            };
            let (index, cleared) = tokio::task::spawn_blocking(move || {
                let before = index.key_count();
                let r = index.clear();
                (index, r.map(|_| before))
            })
            .await
            .ok()?;
            session.return_writer(index).await;
            session.invalidate_views();
            match cleared {
                Ok(before) => {
                    emit_index_changed(app, session.id, "flush", 0, before as u64);
                    if let Err(e) = session.start_default(app.clone()).await {
                        tracing::warn!(id = session.id, "scan after a console flush did not start: {e}");
                    }
                    Some("index cleared".to_string())
                }
                Err(e) => {
                    session.index_stale.store(true, Ordering::Relaxed);
                    Some(format!("the index could not be cleared ({e})"))
                }
            }
        }
        Effect::Unknown => {
            session.index_stale.store(true, Ordering::Relaxed);
            Some("the command may have changed keys it did not name; the index may be stale".to_string())
        }
    }
}
