//! Live index from keyspace notifications (CONCEPT section 7, opt-in),
//! scan options (gentleness profile, replica preference) and the node
//! list of a connection (CONCEPT section 4). The subscription runs in
//! the core; this module applies its event batches to the index writer
//! and tells the webview.

use std::sync::Arc;
use std::time::{Duration, Instant};

use keyfold_core::notify::{self, LiveConfig, LivePhase, LiveStatus};
use keyfold_core::route::Hop;
use keyfold_core::{ScanProfile, ScanTarget, Tag, routed};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::Sessions;
use crate::mutate::emit_index_changed;
use crate::session::Session;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStatusDto {
    pub session: u32,
    /// `off`, `connecting`, `live`, `reconnecting`, `stopped`
    pub phase: &'static str,
    pub nodes_live: usize,
    pub nodes: usize,
    pub events: u64,
    /// Index rows added and removed by notifications in this session.
    pub applied_added: u64,
    pub applied_removed: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifySettingDto {
    /// The server's `notify-keyspace-events`; `None` when `CONFIG GET` is denied.
    pub value: Option<String>,
    /// Key events for every class are on; `None` when unknown.
    pub enabled: Option<bool>,
}

pub(crate) fn status_dto(session: u32, s: &LiveStatus, applied: (u64, u64), on: bool) -> LiveStatusDto {
    LiveStatusDto {
        session,
        phase: if !on {
            "off"
        } else {
            match s.phase {
                LivePhase::Connecting => "connecting",
                LivePhase::Live => "live",
                LivePhase::Reconnecting => "reconnecting",
                LivePhase::Stopped => "stopped",
            }
        },
        nodes_live: s.nodes_live,
        nodes: s.nodes,
        events: s.events,
        applied_added: applied.0,
        applied_removed: applied.1,
        error: s.error.clone(),
    }
}

/// The server's notification setting, read from the seed node.
#[tauri::command]
pub async fn notify_setting(sessions: State<'_, Sessions>, id: u32) -> Result<NotifySettingDto, String> {
    let session = sessions.get(id).await?;
    let setting = session.notify_setting().await?;
    Ok(NotifySettingDto {
        enabled: setting.keyevents_enabled(),
        value: setting.value,
    })
}

/// `CONFIG SET notify-keyspace-events` on every primary, adding key
/// events to whatever is on. A server-wide setting, so always confirmed
/// in the UI; prod refuses without the flag like every other write.
#[tauri::command]
pub async fn notify_enable(sessions: State<'_, Sessions>, id: u32, confirmed: bool) -> Result<String, String> {
    let session = sessions.get(id).await?;
    if !confirmed {
        return Err("changing a server setting needs a confirmation".into());
    }
    let mut router = session.router.lock().await;
    let mut value = String::new();
    for addr in router.primaries() {
        value = routed!(router, Hop::node(&addr), |c| notify::enable(c).await).map_err(|e| e.to_string())?;
    }
    tracing::warn!(id, value, "notify-keyspace-events set");
    Ok(value)
}

/// Switch live updates on or off. Turning them on subscribes on every
/// primary; the status arrives as `live:status`, and each applied batch as
/// `index:changed` with reason `live`.
#[tauri::command]
pub async fn live_set(app: AppHandle, sessions: State<'_, Sessions>, id: u32, on: bool) -> Result<LiveStatusDto, String> {
    let session = sessions.get(id).await?;
    if on {
        session.require_index()?;
        session.start_live(app).await?;
    } else {
        session.stop_live().await;
    }
    Ok(session.live_status())
}

#[tauri::command]
pub async fn live_status(sessions: State<'_, Sessions>, id: u32) -> Result<LiveStatusDto, String> {
    Ok(sessions.get(id).await?.live_status())
}

/// Gentleness profile and replica preference for the next scan. The
/// running scan keeps its settings; the UI offers a rescan.
#[tauri::command]
pub async fn scan_options(
    sessions: State<'_, Sessions>,
    store: State<'_, Arc<crate::profiles::ProfileStore>>,
    id: u32,
    profile: String,
    target: String,
) -> Result<(), String> {
    let session = sessions.get(id).await?;
    let profile = ScanProfile::parse(&profile).ok_or_else(|| format!("unknown scan profile {profile:?}"))?;
    let target = ScanTarget::parse(&target).ok_or_else(|| format!("unknown scan target {target:?}"))?;
    *session.scan_options.lock().map_err(|_| "options poisoned")? = (profile, target);
    tracing::info!(id, profile = profile.as_str(), target = target.as_str(), "scan options");
    crate::profiles::remember_scan_options(&store, &session.profile.id, profile, target).await;
    Ok(())
}

impl Session {
    pub async fn notify_setting(&self) -> Result<notify::Setting, String> {
        let mut router = self.router.lock().await;
        let seed = router.seed().clone();
        routed!(router, Hop::node(&seed), |c| notify::setting(c).await).map_err(|e| e.to_string())
    }

    pub fn live_status(&self) -> LiveStatusDto {
        let on = self.live_on.load(std::sync::atomic::Ordering::Relaxed);
        let status = self.live_last.lock().map(|s| s.clone()).unwrap_or_default();
        let applied = self.live_applied.lock().map(|a| *a).unwrap_or((0, 0));
        status_dto(self.id, &status, applied, on)
    }

    /// Subscribe on every primary and apply the batches to the index as
    /// they arrive (queued while a scan owns the writer).
    pub async fn start_live(self: &Arc<Self>, app: AppHandle) -> Result<(), String> {
        let mut slot = self.live.lock().await;
        if slot.is_some() {
            return Ok(());
        }
        let setting = self.notify_setting().await?;
        if setting.keyevents_enabled() == Some(false) {
            return Err(format!(
                "notifications-off: the server's notify-keyspace-events is {:?}; key events are not published",
                setting.value.unwrap_or_default()
            ));
        }
        let primaries = self.router.lock().await.primaries();
        let (handle, mut events) = notify::start(self.manager.clone(), primaries, self.profile.db, LiveConfig::default());
        let mut status = handle.status();
        *slot = Some(handle);
        drop(slot);
        self.live_on.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut a) = self.live_applied.lock() {
            *a = (0, 0);
        }
        tracing::info!(id = self.id, "live updates on");

        // status forwarder
        let s = Arc::clone(self);
        let app_status = app.clone();
        tokio::spawn(async move {
            loop {
                if status.changed().await.is_err() {
                    break;
                }
                let st = status.borrow().clone();
                if let Ok(mut last) = s.live_last.lock() {
                    *last = st.clone();
                }
                let _ = app_status.emit("live:status", &s.live_status());
                if st.phase == LivePhase::Stopped {
                    break;
                }
            }
        });

        // event applier
        let s = Arc::clone(self);
        tokio::spawn(async move {
            let mut last_emit = Instant::now() - Duration::from_secs(1);
            let mut pending_change = (0u64, 0u64);
            while let Some(batch) = events.recv().await {
                let outcomes = notify::reduce(&batch);
                match s.write_through_many(outcomes).await {
                    Ok((change, queued)) => {
                        if let Ok(mut a) = s.live_applied.lock() {
                            a.0 += change.added;
                            a.1 += change.removed;
                        }
                        pending_change.0 += change.added;
                        pending_change.1 += change.removed;
                        tracing::debug!(
                            id = s.id,
                            events = batch.len(),
                            added = change.added,
                            removed = change.removed,
                            queued,
                            "live batch"
                        );
                    }
                    Err(e) => tracing::warn!("live batch failed: {e}"),
                }
                // tell the webview at most four times a second
                if (pending_change.0 > 0 || pending_change.1 > 0) && last_emit.elapsed() >= Duration::from_millis(250) {
                    emit_index_changed(&app, s.id, "live", pending_change.0, pending_change.1);
                    let _ = app.emit("live:status", &s.live_status());
                    pending_change = (0, 0);
                    last_emit = Instant::now();
                }
            }
            if pending_change.0 > 0 || pending_change.1 > 0 {
                emit_index_changed(&app, s.id, "live", pending_change.0, pending_change.1);
            }
            let _ = app.emit("live:status", &s.live_status());
        });
        Ok(())
    }

    pub async fn stop_live(&self) {
        if let Some(h) = self.live.lock().await.take() {
            h.stop();
        }
        self.live_on.store(false, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(id = self.id, "live updates off");
    }

    pub fn is_prod(&self) -> bool {
        self.profile.tag == Tag::Prod
    }
}
