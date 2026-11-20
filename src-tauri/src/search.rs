//! Search commands (CONCEPT 5.3): windows of matches from the index, an
//! exact count, cancellation, the opt-in trigram table and a server-side
//! verification. Searches run on their own read-only index so the list
//! and tree never wait behind a one-second substring scan.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use keyfold_core::index::MAX_WINDOW;
use keyfold_core::search::{self, Plan, VerifyOptions};
use keyfold_core::{ScanProfile, Tag};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::Sessions;
use crate::wire::{bytes_from, display_and_raw};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRowDto {
    pub name: String,
    /// Exact bytes as hex when the name is not UTF-8.
    pub raw: Option<String>,
    pub ttl: Option<i64>,
    pub mem: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPageDto {
    pub mode: &'static str,
    pub rows: Vec<SearchRowDto>,
    pub done: bool,
    pub cancelled: bool,
    pub via_fts: bool,
    /// The trigram table exists for this index.
    pub fts: bool,
    /// The query could use the trigram table if it existed.
    pub fts_applicable: bool,
    pub examined: u64,
    pub took_ms: f64,
    /// The index can be verified against the server for this query.
    pub verifiable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyDto {
    pub calls: u64,
    pub examined: u64,
    pub matched: u64,
    pub added: u64,
    pub removed: u64,
    pub complete: bool,
    pub truncated: bool,
    pub took_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FtsProgressDto {
    pub session: u32,
    /// `building`, `ready`, `dropping`, `dropped`, `cancelled`, `failed`
    pub phase: &'static str,
    pub done: i64,
    pub total: i64,
    pub error: Option<String>,
}

fn plan_for(query: &str) -> Result<Plan, String> {
    Plan::new(search::parse(query)).map_err(|e| e.to_string())
}

/// One window of matches after `after`. The first window of a query
/// (`fresh`) cancels whatever search or count was still running for the
/// previous one; later windows share the query's flag so they do not
/// abort its count.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn search(
    sessions: State<'_, Sessions>,
    id: u32,
    query: String,
    after: Option<String>,
    after_raw: Option<String>,
    limit: usize,
    fresh: bool,
) -> Result<SearchPageDto, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let plan = plan_for(&query)?;
    let after = match after {
        Some(a) => Some(bytes_from(&a, after_raw.as_deref())?),
        None => None,
    };
    let cancel = if fresh { session.new_search() } else { session.current_search() };
    let t = Instant::now();
    let result = tokio::task::spawn_blocking(move || -> Result<SearchPageDto, String> {
        let mut reader = session.search_reader.lock().map_err(|_| "search reader poisoned")?;
        reader.reload_meta().map_err(|e| e.to_string())?;
        let page = reader
            .search(&plan, after.as_deref(), limit.min(MAX_WINDOW), &cancel)
            .map_err(|e| e.to_string())?;
        Ok(SearchPageDto {
            mode: plan.query.mode.name(),
            rows: page
                .rows
                .iter()
                .map(|r| {
                    let (name, raw) = display_and_raw(&r.name);
                    SearchRowDto {
                        name,
                        raw,
                        ttl: r.ttl,
                        mem: r.mem,
                    }
                })
                .collect(),
            done: page.done,
            cancelled: page.cancelled,
            via_fts: page.via_fts,
            fts: reader.has_fts(),
            fts_applicable: plan.fts_literal.is_some(),
            examined: page.examined,
            took_ms: t.elapsed().as_secs_f64() * 1000.0,
            verifiable: plan.server_pattern().is_some(),
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Ok(p) = &result {
        tracing::debug!(id, query, mode = p.mode, rows = p.rows.len(), done = p.done, cancelled = p.cancelled, via_fts = p.via_fts, examined = p.examined, took = ?t.elapsed(), "search");
    }
    result
}

/// Exact number of matches; `None` when cancelled by a newer search.
#[tauri::command]
pub async fn search_count(sessions: State<'_, Sessions>, id: u32, query: String) -> Result<Option<i64>, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let plan = plan_for(&query)?;
    let cancel = session.current_search();
    let t = Instant::now();
    let result = tokio::task::spawn_blocking(move || -> Result<Option<i64>, String> {
        let reader = session.search_reader.lock().map_err(|_| "search reader poisoned")?;
        reader.search_count(&plan, &cancel).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Ok(n) = &result {
        tracing::debug!(id, query, count = ?n, took = ?t.elapsed(), "search_count");
    }
    result
}

/// Abort the running search, count or verification.
#[tauri::command]
pub async fn search_cancel(sessions: State<'_, Sessions>, id: u32) -> Result<(), String> {
    let session = sessions.get(id).await?;
    session.cancel_search();
    Ok(())
}

/// Build (or drop) the trigram table in the background, from the keys
/// already in the index, with `search:fts` progress events. Needs the
/// index writer, so it waits for no scan to be running.
#[tauri::command]
pub async fn search_fts(app: AppHandle, sessions: State<'_, Sessions>, id: u32, enable: bool) -> Result<(), String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    if session.is_scanning().await {
        return Err("wait for the scan to finish".into());
    }
    let Some(mut index) = session.take_writer().await else {
        return Err("index is busy".into());
    };
    session.fts_cancel.store(false, Ordering::Relaxed);
    let s = Arc::clone(&session);
    tokio::spawn(async move {
        let app2 = app.clone();
        let sid = s.id;
        let cancel = Arc::clone(&s.fts_cancel);
        let t = Instant::now();
        let (index, outcome) = tokio::task::spawn_blocking(move || {
            let emit = |phase: &'static str, done: i64, total: i64, error: Option<String>| {
                let _ = app2.emit(
                    "search:fts",
                    &FtsProgressDto {
                        session: sid,
                        phase,
                        done,
                        total,
                        error,
                    },
                );
            };
            let r = if enable {
                let mut last = Instant::now() - Duration::from_secs(1);
                emit("building", 0, index.key_count(), None);
                index
                    .build_fts(&mut |done, total| {
                        if last.elapsed() >= Duration::from_millis(250) {
                            emit("building", done, total, None);
                            last = Instant::now();
                        }
                        !cancel.load(Ordering::Relaxed)
                    })
                    .map(|built| if built { "ready" } else { "cancelled" })
            } else {
                emit("dropping", 0, 0, None);
                index.drop_fts().map(|_| "dropped")
            };
            let outcome = match r {
                Ok(phase) => {
                    emit(phase, index.key_count(), index.key_count(), None);
                    Ok(phase.to_string())
                }
                Err(e) => {
                    emit("failed", 0, 0, Some(e.to_string()));
                    Err(e.to_string())
                }
            };
            (index, outcome)
        })
        .await
        .unwrap_or_else(|e| panic!("fts task failed: {e}"));
        tracing::info!(id = sid, enable, outcome = ?outcome, took = ?t.elapsed(), "search_fts");
        s.return_writer(index).await;
    });
    Ok(())
}

/// Walk the server with `SCAN MATCH` for this query and reconcile the
/// index (CONCEPT 5.3 "verify against server"). Runs on its own
/// connection with the index writer, so it needs no scan to be running.
#[tauri::command]
pub async fn search_verify(sessions: State<'_, Sessions>, id: u32, query: String) -> Result<VerifyDto, String> {
    let session = sessions.get(id).await?;
    session.require_index()?;
    let plan = plan_for(&query)?;
    let pattern = plan
        .server_pattern()
        .ok_or("regular expressions cannot be verified on the server")?;
    if session.is_scanning().await {
        return Err("wait for the scan to finish".into());
    }
    let Some(mut index) = session.take_writer().await else {
        return Err("index is busy".into());
    };
    let mut router = match session.manager.connect().await {
        Ok(c) => keyfold_core::Router::new(session.manager.clone(), c, &session.caps).await,
        Err(e) => {
            session.return_writer(index).await;
            return Err(e.to_string());
        }
    };
    let opts = VerifyOptions {
        gap: if session.profile.tag == Tag::Prod {
            ScanProfile::Polite.gap()
        } else {
            Duration::ZERO
        },
        ..VerifyOptions::default()
    };
    let cancel = session.current_search();
    let t = Instant::now();
    let result = search::verify(&mut router, &mut index, &pattern, &opts, &cancel).await;
    session.return_writer(index).await;
    let report = result.map_err(|e| e.to_string())?;
    tracing::info!(id, query, ?report, took = ?t.elapsed(), "search_verify");
    if report.added > 0 || report.removed > 0 {
        session.invalidate_views();
    }
    Ok(VerifyDto {
        calls: report.calls,
        examined: report.examined,
        matched: report.matched,
        added: report.added,
        removed: report.removed,
        complete: report.complete,
        truncated: report.truncated,
        took_ms: t.elapsed().as_secs_f64() * 1000.0,
    })
}
