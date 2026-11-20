//! Keyspace notifications (CONCEPT section 7, opt-in): when the server
//! publishes `__keyevent@<db>__:*`, a subscription keeps the index live for
//! new, deleted, renamed and expired keys. Off by default; it costs server
//! CPU and needs `notify-keyspace-events` to contain `E` plus the key
//! classes, which the user may ask the app to set.
//!
//! Subscriptions are the one place the client crate's typed API is used
//! (its push stream); the commands it sends are `PSUBSCRIBE` and
//! `PUNSUBSCRIBE`, listed in `docs/commands.md`.

use std::time::Duration;

use futures_util::StreamExt;
use redis::Value;
use tokio::sync::{mpsc, watch};

use crate::capability::value_text;
use crate::cluster::NodeAddr;
use crate::commands;
use crate::connection::{Conn, ConnectionManager};
use crate::error::Result;

pub const PARAMETER: &str = "notify-keyspace-events";

/// What `CONFIG GET notify-keyspace-events` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// `None` when `CONFIG GET` is denied: the subscription is tried anyway.
    pub value: Option<String>,
}

impl Setting {
    /// Key-event notifications for every class the index cares about are
    /// on: `E` with `A`, or `E` with the generic class and every type
    /// class.
    pub fn keyevents_enabled(&self) -> Option<bool> {
        self.value.as_deref().map(flags_cover_keyevents)
    }

    /// The value to set so that keyevents are published, keeping whatever
    /// else was on.
    pub fn enabling_value(&self) -> String {
        enabling_flags(self.value.as_deref().unwrap_or(""))
    }
}

pub fn flags_cover_keyevents(flags: &str) -> bool {
    let has = |c: char| flags.contains(c);
    has('E') && (has('A') || (has('g') && has('$') && has('l') && has('s') && has('h') && has('z') && has('x') && has('e') && has('t')))
}

/// `E` plus `A`, merged into the current flags without duplicates.
pub fn enabling_flags(current: &str) -> String {
    let mut out: String = current.chars().filter(|c| !c.is_whitespace()).collect();
    for c in ['E', 'A'] {
        if !out.contains(c) {
            out.push(c);
        }
    }
    out
}

/// Read the setting from one node.
pub async fn setting(conn: &mut Conn) -> Result<Setting> {
    match conn.raw(&commands::config_get(PARAMETER)).await {
        Ok(v) => Ok(Setting {
            value: Some(config_value(&v)),
        }),
        Err(e) if e.is_denied() => Ok(Setting { value: None }),
        Err(e) => Err(e),
    }
}

/// `CONFIG GET` answers `[name, value]` in RESP2 and a map in RESP3.
pub(crate) fn config_value(v: &Value) -> String {
    match v {
        Value::Map(items) => items.first().map(|(_, v)| value_text(v)).unwrap_or_default(),
        Value::Array(items) => items.get(1).map(value_text).unwrap_or_default(),
        other => value_text(other),
    }
}

/// Turn keyevent notifications on at one node, keeping its other flags.
/// Returns the value that was set.
pub async fn enable(conn: &mut Conn) -> Result<String> {
    let current = setting(conn).await?;
    let value = current.enabling_value();
    conn.raw(&commands::config_set(PARAMETER, &value)).await?;
    Ok(value)
}

/// One keyspace event, reduced to what the index needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub key: Vec<u8>,
    /// The key no longer exists (`del`, `expired`, `evicted`, `rename_from`).
    pub gone: bool,
    pub event: String,
}

/// Classify the event name of a `__keyevent@N__:<event>` channel.
pub fn parse_channel(channel: &str) -> Option<(i64, &str)> {
    let rest = channel.strip_prefix("__keyevent@")?;
    let (db, event) = rest.split_once("__:")?;
    Some((db.parse().ok()?, event))
}

pub fn is_gone(event: &str) -> bool {
    matches!(event, "del" | "expired" | "evicted" | "rename_from" | "move_from" | "migrate_from")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LivePhase {
    #[default]
    Connecting,
    Live,
    /// A subscription dropped; it is retried with backoff.
    Reconnecting,
    Stopped,
}

impl LivePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            LivePhase::Connecting => "connecting",
            LivePhase::Live => "live",
            LivePhase::Reconnecting => "reconnecting",
            LivePhase::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LiveStatus {
    pub phase: LivePhase,
    /// Nodes subscribed right now, of the nodes asked for.
    pub nodes_live: usize,
    pub nodes: usize,
    /// Events received since the subscription started.
    pub events: u64,
    pub error: Option<String>,
}

/// A running subscription over one or more nodes.
pub struct LiveHandle {
    stop: watch::Sender<bool>,
    status: watch::Receiver<LiveStatus>,
}

impl LiveHandle {
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    pub fn status(&self) -> watch::Receiver<LiveStatus> {
        self.status.clone()
    }
}

#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Events are handed over in batches: every this often ...
    pub flush_every: Duration,
    /// ... or when this many are waiting.
    pub flush_at: usize,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            flush_every: Duration::from_millis(250),
            flush_at: 2000,
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(30),
        }
    }
}

/// Subscribe to `__keyevent@<db>__:*` on every node given (the primaries
/// of a cluster, or the one server). Events arrive on the returned channel
/// in batches; the handle reports status and stops the tasks.
pub fn start(manager: ConnectionManager, nodes: Vec<NodeAddr>, db: i64, config: LiveConfig) -> (LiveHandle, mpsc::Receiver<Vec<Event>>) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (status_tx, status_rx) = watch::channel(LiveStatus {
        nodes: nodes.len(),
        ..Default::default()
    });
    let (events_tx, events_rx) = mpsc::channel::<Vec<Event>>(64);
    let (node_tx, mut node_rx) = mpsc::unbounded_channel::<NodeReport>();
    for (i, addr) in nodes.iter().cloned().enumerate() {
        let task = NodeSub {
            idx: i,
            addr,
            db,
            manager: manager.clone(),
            config: config.clone(),
            stop: stop_rx.clone(),
            events: events_tx.clone(),
            report: node_tx.clone(),
        };
        tokio::spawn(task.run());
    }
    drop(node_tx);
    drop(events_tx);
    let total = nodes.len();
    tokio::spawn(async move {
        let mut live = vec![false; total];
        let mut lost_once = false;
        let mut status = LiveStatus {
            nodes: total,
            ..Default::default()
        };
        while let Some(r) = node_rx.recv().await {
            match r {
                NodeReport::Live(i) => {
                    live[i] = true;
                    if live.iter().all(|l| *l) {
                        status.error = None;
                    }
                }
                NodeReport::Lost(i, e) => {
                    live[i] = false;
                    lost_once = true;
                    status.error = Some(e);
                }
                NodeReport::Events(n) => status.events += n,
            }
            status.nodes_live = live.iter().filter(|l| **l).count();
            status.phase = if status.nodes_live == total {
                LivePhase::Live
            } else if lost_once {
                LivePhase::Reconnecting
            } else {
                LivePhase::Connecting
            };
            let _ = status_tx.send_replace(status.clone());
        }
        status.phase = LivePhase::Stopped;
        status.nodes_live = 0;
        let _ = status_tx.send_replace(status);
    });
    (
        LiveHandle {
            stop: stop_tx,
            status: status_rx,
        },
        events_rx,
    )
}

enum NodeReport {
    Live(usize),
    Lost(usize, String),
    Events(u64),
}

struct NodeSub {
    idx: usize,
    addr: NodeAddr,
    db: i64,
    manager: ConnectionManager,
    config: LiveConfig,
    stop: watch::Receiver<bool>,
    events: mpsc::Sender<Vec<Event>>,
    report: mpsc::UnboundedSender<NodeReport>,
}

impl NodeSub {
    async fn run(mut self) {
        let mut backoff = self.config.reconnect_min;
        loop {
            if *self.stop.borrow() {
                return;
            }
            match self.subscribe_once().await {
                Ok(()) => return,
                Err(e) => {
                    let _ = self.report.send(NodeReport::Lost(self.idx, e.clone()));
                    tracing::warn!(node = %self.addr, "keyspace subscription lost: {e}; retrying in {backoff:?}");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.stop.changed() => return,
            }
            backoff = (backoff * 2).min(self.config.reconnect_max);
        }
    }

    /// The subscription through an SSH tunnel: a RESP3 connection whose
    /// push messages are the events (see
    /// [`ConnectionManager::subscribe_pattern`]).
    async fn subscribe_through_tunnel(&mut self) -> std::result::Result<(), String> {
        let pattern = format!("__keyevent@{}__:*", self.db);
        let (_conn, mut pushes) = tokio::time::timeout(Duration::from_secs(20), self.manager.subscribe_pattern(&self.addr, &pattern))
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let _ = self.report.send(NodeReport::Live(self.idx));
        let mut batch: Vec<Event> = Vec::new();
        let mut flush = tokio::time::interval(self.config.flush_every);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                info = pushes.recv() => {
                    let lost = match &info {
                        None => true,
                        Some(i) => i.kind == redis::PushKind::Disconnection,
                    };
                    if lost {
                        self.flush(&mut batch).await;
                        return Err("subscription connection lost".into());
                    }
                    if let Some(msg) = info.and_then(redis::Msg::from_push_info)
                        && let Some((_, event)) = parse_channel(msg.get_channel_name())
                    {
                        batch.push(Event {
                            key: msg.get_payload_bytes().to_vec(),
                            gone: is_gone(event),
                            event: event.to_string(),
                        });
                        if batch.len() >= self.config.flush_at {
                            self.flush(&mut batch).await;
                        }
                    }
                }
                _ = flush.tick() => {
                    self.flush(&mut batch).await;
                }
                _ = self.stop.changed() => {
                    self.flush(&mut batch).await;
                    return Ok(());
                }
            }
        }
    }

    /// One subscription until it ends. `Ok` means stopped on request.
    async fn subscribe_once(&mut self) -> std::result::Result<(), String> {
        if self.manager.tunnel().is_some() {
            return self.subscribe_through_tunnel().await;
        }
        let client = self.manager.pubsub_client(&self.addr).map_err(|e| e.to_string())?;
        let mut pubsub = tokio::time::timeout(Duration::from_secs(10), client.get_async_pubsub())
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let pattern = format!("__keyevent@{}__:*", self.db);
        pubsub.psubscribe(&pattern).await.map_err(|e| e.to_string())?;
        let _ = self.report.send(NodeReport::Live(self.idx));
        let mut stream = pubsub.into_on_message();
        let mut batch: Vec<Event> = Vec::new();
        let mut flush = tokio::time::interval(self.config.flush_every);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = stream.next() => {
                    let Some(msg) = msg else {
                        self.flush(&mut batch).await;
                        return Err("subscription stream ended".into());
                    };
                    if let Some((_, event)) = parse_channel(msg.get_channel_name()) {
                        batch.push(Event {
                            key: msg.get_payload_bytes().to_vec(),
                            gone: is_gone(event),
                            event: event.to_string(),
                        });
                        if batch.len() >= self.config.flush_at {
                            self.flush(&mut batch).await;
                        }
                    }
                }
                _ = flush.tick() => {
                    self.flush(&mut batch).await;
                }
                _ = self.stop.changed() => {
                    self.flush(&mut batch).await;
                    return Ok(());
                }
            }
        }
    }

    async fn flush(&mut self, batch: &mut Vec<Event>) {
        if batch.is_empty() {
            return;
        }
        let n = batch.len() as u64;
        let out = std::mem::take(batch);
        if self.events.send(out).await.is_ok() {
            let _ = self.report.send(NodeReport::Events(n));
        }
    }
}

/// Reduce a batch to one state per key, in arrival order of the last
/// event: an `Outcome` list for the index writer.
pub fn reduce(events: &[Event]) -> Vec<crate::mutate::Outcome> {
    use std::collections::HashMap;
    let mut last: HashMap<&[u8], bool> = HashMap::new();
    let mut order: Vec<&[u8]> = Vec::new();
    for e in events {
        if last.insert(e.key.as_slice(), e.gone).is_none() {
            order.push(e.key.as_slice());
        }
    }
    order
        .into_iter()
        .map(|k| {
            let gone = last[k];
            crate::mutate::Outcome {
                removed: if gone { vec![k.to_vec()] } else { Vec::new() },
                added: if gone { Vec::new() } else { vec![k.to_vec()] },
                ttl: None,
                reply: String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_recognised_and_merged() {
        assert!(flags_cover_keyevents("KEA"));
        assert!(flags_cover_keyevents("EA"));
        assert!(flags_cover_keyevents("Eg$lshzxet"));
        assert!(!flags_cover_keyevents("KA"));
        assert!(!flags_cover_keyevents("Eg$"));
        assert!(!flags_cover_keyevents(""));
        assert_eq!(enabling_flags(""), "EA");
        assert_eq!(enabling_flags("K"), "KEA");
        assert_eq!(enabling_flags("KEA"), "KEA");
        assert_eq!(enabling_flags("Ex"), "ExA");
        assert_eq!(Setting { value: Some("".into()) }.keyevents_enabled(), Some(false));
        assert_eq!(Setting { value: None }.keyevents_enabled(), None);
    }

    #[test]
    fn channels_and_events_are_classified() {
        assert_eq!(parse_channel("__keyevent@0__:del"), Some((0, "del")));
        assert_eq!(parse_channel("__keyevent@15__:rename_to"), Some((15, "rename_to")));
        assert_eq!(parse_channel("__keyspace@0__:mykey"), None);
        assert!(is_gone("expired"));
        assert!(is_gone("rename_from"));
        assert!(!is_gone("rename_to"));
        assert!(!is_gone("hset"));
    }

    #[test]
    fn a_batch_reduces_to_the_last_state_per_key() {
        let ev = |k: &str, e: &str| Event {
            key: k.as_bytes().to_vec(),
            gone: is_gone(e),
            event: e.into(),
        };
        let out = reduce(&[ev("a", "set"), ev("b", "set"), ev("a", "del"), ev("c", "expired"), ev("c", "set")]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].removed, vec![b"a".to_vec()]);
        assert_eq!(out[1].added, vec![b"b".to_vec()]);
        assert_eq!(out[2].added, vec![b"c".to_vec()]);
    }
}
