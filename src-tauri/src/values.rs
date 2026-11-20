//! Value commands (CONCEPT section 6) and the type registry that maps a
//! server's exact `TYPE` string to a viewer. Unknown types get the raw
//! viewer, never an error.

use keyfold_core::route::Hop;
use keyfold_core::routed;
use keyfold_core::value::{self, CHUNK_BYTES, Cursor, Kind, PAGE, PREVIEW_BYTES};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::Sessions;
use crate::wire::{KeyArg, hex, text_and_raw};

/// Viewer for a `TYPE` string. Both server ecosystems' module types land
/// here as entries later; today the native types have viewers.
pub fn viewer_for(key_type: &str) -> &'static str {
    match key_type {
        "string" => "string",
        "hash" => "hash",
        "set" => "set",
        "zset" => "zset",
        "list" => "list",
        "stream" => "stream",
        _ => "raw",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfoDto {
    pub exists: bool,
    pub key_type: String,
    pub viewer: &'static str,
    pub ttl: Option<i64>,
    pub mem: Option<i64>,
    pub encoding: Option<String>,
    pub length: Option<i64>,
    /// For strings: content kind decided on the preview, and whether the
    /// preview already is the whole value.
    pub kind: Option<&'static str>,
    pub preview_complete: bool,
    pub preview_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDto {
    pub field: String,
    pub field_raw: Option<String>,
    pub value: String,
    pub value_raw: Option<String>,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValuePageDto {
    pub entries: Vec<EntryDto>,
    pub cursor: u64,
    pub next: Option<String>,
    pub done: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum CursorDto {
    Scan(u64),
    Rank(i64),
    StreamId(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpDto {
    pub length: usize,
    pub peek_hex: String,
}

/// Selection: one pipeline for type, TTL, memory, encoding and length;
/// strings also get their preview classified. A key that is gone is
/// removed from the index when no scan holds it (verify on open).
#[tauri::command]
pub async fn value_inspect(sessions: State<'_, Sessions>, id: u32, key: KeyArg) -> Result<KeyInfoDto, String> {
    let session = sessions.get(id).await?;
    let t = std::time::Instant::now();
    let key = key.bytes()?;
    let k = &key[..];
    let info = {
        let mut router = session.router.lock().await;
        routed!(router, Hop::key(k), |c| value::inspect(c, k).await).map_err(|e| e.to_string())?
    };
    let (kind, preview_complete, preview_bytes) = if info.exists && info.key_type == "string" {
        let len = info.length.unwrap_or(0);
        let mut router = session.router.lock().await;
        let preview = routed!(router, Hop::key(k), |c| value::string_chunk(c, k, 0, PREVIEW_BYTES).await).map_err(|e| e.to_string())?;
        let complete = (preview.len() as i64) >= len;
        (Some(value::detect(&preview, complete).name()), complete, preview.len() as i64)
    } else {
        (None, true, 0)
    };
    if !info.exists {
        session.forget_key(key.clone()).await;
    }
    tracing::debug!(id, key_type = %info.key_type, exists = info.exists, length = ?info.length, ?kind, took = ?t.elapsed(), "value_inspect");
    Ok(KeyInfoDto {
        exists: info.exists,
        viewer: viewer_for(&info.key_type),
        key_type: info.key_type,
        ttl: info.ttl,
        mem: info.mem,
        encoding: info.encoding,
        length: info.length,
        kind,
        preview_complete,
        preview_bytes,
    })
}

/// One page of a collection (CONCEPT 6: `HSCAN`, `SSCAN`, `ZSCAN`,
/// `ZRANGE` windows, `LRANGE` windows, `XRANGE` with a bounded count).
#[tauri::command]
pub async fn value_page(
    sessions: State<'_, Sessions>,
    id: u32,
    key: KeyArg,
    key_type: String,
    cursor: Option<CursorDto>,
    limit: usize,
    filter: Option<String>,
) -> Result<ValuePageDto, String> {
    let session = sessions.get(id).await?;
    let cursor = match cursor {
        None => Cursor::Start,
        Some(CursorDto::Scan(n)) => Cursor::Scan(n),
        Some(CursorDto::Rank(r)) => Cursor::Rank(r),
        Some(CursorDto::StreamId(s)) => Cursor::StreamId(s),
    };
    let filter = filter.filter(|f| !f.is_empty());
    let key = key.bytes()?;
    let t = std::time::Instant::now();
    let page = {
        let k = &key[..];
        let f = filter.as_deref().map(str::as_bytes);
        let mut router = session.router.lock().await;
        routed!(router, Hop::key(k), |c| value::page(
            c,
            k,
            &key_type,
            cursor.clone(),
            limit.min(PAGE),
            f
        )
        .await)
        .map_err(|e| e.to_string())?
    };
    tracing::debug!(id, key_type, entries = page.entries.len(), done = page.done, took = ?t.elapsed(), "value_page");
    Ok(ValuePageDto {
        entries: page
            .entries
            .into_iter()
            .map(|e| {
                let (field, field_raw) = text_and_raw(&e.field);
                let (value, value_raw) = text_and_raw(&e.value);
                EntryDto {
                    field,
                    field_raw,
                    value,
                    value_raw,
                    score: e.score,
                }
            })
            .collect(),
        cursor: page.cursor,
        next: page.next,
        done: page.done,
    })
}

/// A slice of a string value as raw bytes over the IPC (no JSON encoding).
#[tauri::command]
pub async fn value_chunk(
    sessions: State<'_, Sessions>,
    id: u32,
    key: KeyArg,
    offset: i64,
    len: i64,
) -> Result<tauri::ipc::Response, String> {
    let session = sessions.get(id).await?;
    let key = key.bytes()?;
    let k = &key[..];
    let mut router = session.router.lock().await;
    let bytes = routed!(router, Hop::key(k), |c| value::string_chunk(
        c,
        k,
        offset.max(0),
        len.clamp(1, CHUNK_BYTES)
    )
    .await)
    .map_err(|e| e.to_string())?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Raw fallback for a type without a viewer.
#[tauri::command]
pub async fn value_dump(sessions: State<'_, Sessions>, id: u32, key: KeyArg) -> Result<DumpDto, String> {
    let session = sessions.get(id).await?;
    let key = key.bytes()?;
    let k = &key[..];
    let mut router = session.router.lock().await;
    let (length, peek) = routed!(router, Hop::key(k), |c| value::dump_peek(c, k, 256).await).map_err(|e| e.to_string())?;
    Ok(DumpDto {
        length,
        peek_hex: hex(&peek),
    })
}

#[allow(dead_code)]
fn _kinds_are_named(k: Kind) -> &'static str {
    k.name()
}
