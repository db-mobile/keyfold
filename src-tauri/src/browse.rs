//! Browsing without an index (CONCEPT 7, "no index"): pages of plain
//! `SCAN`, read from the nodes a scan would read, with the scan profile's
//! gap between calls. The walk lives in the session; the webview asks for
//! the next page or starts over.

use keyfold_core::browse::{PageOptions, Pager};
use keyfold_core::scan;
use keyfold_core::search::{self, Plan};
use serde::Serialize;
use tauri::State;

use crate::Sessions;
use crate::wire::KeyDto;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeysPageDto {
    pub keys: Vec<KeyDto>,
    /// Every node was walked to the end.
    pub done: bool,
    /// `SCAN` calls of this page.
    pub calls: u32,
    /// Keys handed out since the walk began.
    pub returned: u64,
    /// About how many keys the server has walked since the walk began.
    pub examined: u64,
    /// The node being read and its place among the nodes.
    pub node: Option<String>,
    pub node_at: usize,
    pub nodes: usize,
    /// `glob` or `substring` when a pattern filters the walk.
    pub mode: Option<&'static str>,
    pub took_ms: f64,
}

/// The next page of key names. `restart` begins a new walk; so does a
/// `pattern` other than the one the walk runs with. The pattern is what
/// the search box holds: a glob as it is, plain text as a substring; a
/// regular expression needs the index.
#[tauri::command]
pub async fn keys_page(sessions: State<'_, Sessions>, id: u32, pattern: Option<String>, restart: bool) -> Result<KeysPageDto, String> {
    let session = sessions.get(id).await?;
    let text = pattern.unwrap_or_default().trim().to_string();
    let (profile, target) = *session.scan_options.lock().map_err(|_| "options poisoned")?;
    let t = std::time::Instant::now();
    let mut slot = session.pager.lock().await;
    let mut mode = None;
    let glob = if text.is_empty() {
        None
    } else {
        let plan = Plan::new(search::parse(&text)).map_err(|e| e.to_string())?;
        mode = Some(plan.query.mode.name());
        Some(
            plan.server_pattern()
                .ok_or("a regular expression cannot run on the server; it needs an index")?,
        )
    };
    if restart || slot.as_ref().is_none_or(|(_, running)| *running != text) {
        let plan = {
            let router = session.router.lock().await;
            scan::targets(&session.manager, &session.caps, router.topology(), target, profile).await
        }
        .map_err(|e| e.to_string())?;
        tracing::info!(id, nodes = %scan::describe(&plan.nodes), pattern = %text, "keys walk");
        *slot = Some((Pager::new(plan.nodes, glob, None), text));
    }
    let (pager, _) = slot.as_mut().ok_or("no walk")?;
    let opts = PageOptions {
        gap: profile.gap(),
        ..PageOptions::default()
    };
    let page = match pager.next_page(&opts).await {
        Ok(p) => p,
        Err(e) => {
            // the connections of the walk may be gone: the next call starts over
            *slot = None;
            return Err(e.to_string());
        }
    };
    let (node, node_at, nodes) = pager.position();
    let dto = KeysPageDto {
        keys: page.keys.iter().map(|k| KeyDto::new(k)).collect(),
        done: page.done,
        calls: page.calls,
        returned: pager.returned(),
        examined: pager.examined(),
        node: node.map(str::to_string),
        node_at,
        nodes,
        mode,
        took_ms: t.elapsed().as_secs_f64() * 1000.0,
    };
    tracing::debug!(id, keys = dto.keys.len(), calls = dto.calls, done = dto.done, examined = dto.examined, took = ?t.elapsed(), "keys_page");
    Ok(dto)
}
