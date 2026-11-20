//! Per-key metadata for visible rows: `TYPE`, `TTL`, `MEMORY USAGE`, in one
//! pipeline. Nothing here is fetched on hover; callers batch per scroll stop.

use redis::Value;

use crate::capability::value_text;
use crate::commands;
use crate::connection::Conn;
use crate::error::Result;
use crate::route::{Hop, Router};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyMeta {
    /// `false` when `TYPE` answered `none`: the key is gone.
    pub exists: bool,
    /// The exact `TYPE` string, for the type registry.
    pub key_type: Option<String>,
    /// Seconds; `None` when the key has no expiry or is gone.
    pub ttl: Option<i64>,
    /// Bytes from `MEMORY USAGE`; `None` when denied or gone.
    pub mem: Option<i64>,
}

/// Fetch metadata for up to a window of keys with three pipelined commands
/// per key. A denied `MEMORY USAGE` leaves `mem` empty; nothing else fails.
pub async fn fetch(conn: &mut Conn, keys: &[&[u8]]) -> Result<Vec<KeyMeta>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipe = commands::pipeline();
    for k in keys {
        pipe.add_command(commands::key_type(k));
        pipe.add_command(commands::ttl(k));
        pipe.add_command(commands::memory_usage(k));
    }
    pipe.ignore_errors();
    let replies = conn.pipeline(&pipe).await?;
    let mut out = Vec::with_capacity(keys.len());
    for chunk in replies.chunks(3) {
        let key_type = chunk.first().and_then(|r| r.as_ref().ok()).map(value_text);
        let exists = key_type.as_deref().is_some_and(|t| t != "none");
        let ttl = match chunk.get(1).and_then(|r| r.as_ref().ok()) {
            Some(Value::Int(n)) if *n >= 0 => Some(*n),
            _ => None,
        };
        let mem = match chunk.get(2).and_then(|r| r.as_ref().ok()) {
            Some(Value::Int(n)) if *n >= 0 => Some(*n),
            _ => None,
        };
        out.push(KeyMeta {
            exists,
            key_type: if exists { key_type } else { None },
            ttl: if exists { ttl } else { None },
            mem: if exists { mem } else { None },
        });
    }
    Ok(out)
}

/// [`fetch`] over a router: one pipeline per node that owns some of the
/// keys, results back in the caller's order.
pub async fn fetch_routed(router: &mut Router, keys: &[&[u8]]) -> Result<Vec<KeyMeta>> {
    if !router.is_cluster() {
        let seed = router.seed().clone();
        return crate::routed!(router, Hop::node(&seed), |c| fetch(c, keys).await);
    }
    let mut out = vec![KeyMeta::default(); keys.len()];
    for (addr, idxs) in router.group_by_node(keys) {
        let subset: Vec<&[u8]> = idxs.iter().map(|&i| keys[i]).collect();
        let metas = crate::routed!(router, Hop::node(&addr), |c| fetch(c, &subset).await)?;
        for (i, m) in idxs.into_iter().zip(metas) {
            out[i] = m;
        }
    }
    Ok(out)
}
