//! Capability layer: what this server is and which commands it offers.
//!
//! Nothing here, and nothing anywhere else, gates a feature on
//! `redis_version`. Valkey reports `redis_version: 7.2.4` forever. Identity
//! comes from `INFO server`, availability from `COMMAND INFO`, and denial by
//! ACL is learned from the first `NOPERM` and remembered.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use redis::Value;

use crate::cluster::NodeAddr;
use crate::commands;
use crate::connection::Conn;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerKind {
    Redis,
    Valkey,
    Other(String),
}

impl std::fmt::Display for ServerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerKind::Redis => f.write_str("redis"),
            ServerKind::Valkey => f.write_str("valkey"),
            ServerKind::Other(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandState {
    Available,
    Missing,
    Denied,
    /// `COMMAND` itself was unavailable; assume present and learn on use.
    Unknown,
}

/// Commands whose availability is probed on connect. Subcommands use the
/// pipe notation that `COMMAND INFO` understands on both servers.
pub const PROBED_COMMANDS: &[&str] = &[
    "scan",
    "type",
    "ttl",
    "exists",
    "dbsize",
    "info",
    "memory|usage",
    "object|encoding",
    "unlink",
    "cluster|shards",
    "cluster|slots",
    "hscan",
    "sscan",
    "zscan",
    "lrange",
    "xrange",
    "getrange",
    "readonly",
    "config|get",
    "config|set",
    "slowlog|len",
    "psubscribe",
];

/// A replica the primary lists in `INFO replication`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaInfo {
    pub addr: NodeAddr,
    /// `online` once the initial sync is through.
    pub state: String,
}

impl ReplicaInfo {
    pub fn online(&self) -> bool {
        self.state == "online"
    }
}

/// Commands the server's ACL refused while the session ran, learned from
/// `NOPERM` replies. Shared: every connection of a [`ConnectionManager`]
/// writes to the same set, and the [`Capabilities`] probed through one of
/// them read it.
///
/// [`ConnectionManager`]: crate::connection::ConnectionManager
#[derive(Debug, Clone, Default)]
pub struct Denials(Arc<Mutex<BTreeSet<String>>>);

impl Denials {
    /// `name` in the pipe notation of [`PROBED_COMMANDS`], any case.
    pub fn mark(&self, name: &str) {
        if let Ok(mut set) = self.0.lock()
            && set.insert(name.to_ascii_lowercase())
        {
            tracing::warn!(command = name, "denied by the server's ACL; remembered for this session");
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.lock().map(|set| set.contains(name)).unwrap_or(false)
    }

    pub fn list(&self) -> Vec<String> {
        self.0.lock().map(|set| set.iter().cloned().collect()).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub server: ServerKind,
    /// Version string of the server's own project, for display only.
    pub version: String,
    pub mode: String,
    pub role: String,
    pub protocol: u8,
    pub db: i64,
    pub dbsize: Option<u64>,
    pub used_memory: Option<u64>,
    pub commands: BTreeMap<String, CommandState>,
    /// What the ACL refused since the probe.
    pub denied: Denials,
    /// Replicas of a standalone primary, from `INFO replication`.
    pub replicas: Vec<ReplicaInfo>,
    /// The primary of a standalone replica.
    pub primary: Option<NodeAddr>,
}

impl Capabilities {
    pub fn command(&self, name: &str) -> CommandState {
        if self.denied.contains(name) {
            return CommandState::Denied;
        }
        self.commands.get(name).copied().unwrap_or(CommandState::Unknown)
    }

    pub fn has(&self, name: &str) -> bool {
        matches!(self.command(name), CommandState::Available | CommandState::Unknown)
    }

    /// Remember a `NOPERM` so the feature is no longer offered or tried.
    /// The connections do this themselves for every denied command.
    pub fn mark_denied(&self, name: &str) {
        self.denied.mark(name);
    }

    pub fn is_replica(&self) -> bool {
        self.role == "slave" || self.role == "replica"
    }

    /// The server runs in cluster mode (from `INFO server`, never from a
    /// version number).
    pub fn is_cluster(&self) -> bool {
        self.mode == "cluster"
    }
}

/// Probe a freshly opened connection. Four commands in one pipeline; each
/// may be denied independently and the result degrades per field.
pub async fn probe(conn: &mut Conn, db: i64) -> Result<Capabilities> {
    let mut pipe = commands::pipeline();
    pipe.add_command(commands::info("server"));
    pipe.add_command(commands::info("replication"));
    pipe.add_command(commands::info("memory"));
    pipe.add_command(commands::dbsize());
    pipe.add_command(commands::command_info(PROBED_COMMANDS));
    pipe.ignore_errors();
    let mut replies = conn.pipeline(&pipe).await?.into_iter();
    let server = replies.next().unwrap_or(Err(Error::Scan("missing reply".into())));
    let replication = replies.next().unwrap_or(Err(Error::Scan("missing reply".into())));
    let memory = replies.next().unwrap_or(Err(Error::Scan("missing reply".into())));
    let dbsize = replies.next().unwrap_or(Err(Error::Scan("missing reply".into())));
    let command_info = replies.next().unwrap_or(Err(Error::Scan("missing reply".into())));

    let server_info = server.map(|v| parse_info(&value_text(&v))).unwrap_or_default();
    let repl_info = replication.map(|v| parse_info(&value_text(&v))).unwrap_or_default();
    let mem_info = memory.map(|v| parse_info(&value_text(&v))).unwrap_or_default();

    let (kind, version) = identify(&server_info);
    let mode = server_info
        .get("server_mode")
        .or_else(|| server_info.get("redis_mode"))
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let role = repl_info.get("role").cloned().unwrap_or_else(|| "unknown".into());

    let dbsize = match dbsize {
        Ok(Value::Int(n)) if n >= 0 => Some(n as u64),
        _ => None,
    };
    let used_memory = mem_info.get("used_memory").and_then(|s| s.parse().ok());
    let replicas = parse_replicas(&repl_info);
    let primary = match (
        repl_info.get("master_host"),
        repl_info.get("master_port").and_then(|p| p.parse().ok()),
    ) {
        (Some(h), Some(port)) if role != "master" => Some(NodeAddr { host: h.clone(), port }),
        _ => None,
    };

    let mut cmds = BTreeMap::new();
    match command_info {
        Ok(Value::Array(items)) if items.len() == PROBED_COMMANDS.len() => {
            for (name, item) in PROBED_COMMANDS.iter().zip(items) {
                let state = if matches!(item, Value::Nil) {
                    CommandState::Missing
                } else {
                    CommandState::Available
                };
                cmds.insert((*name).to_string(), state);
            }
        }
        _ => {
            for name in PROBED_COMMANDS {
                cmds.insert((*name).to_string(), CommandState::Unknown);
            }
        }
    }

    Ok(Capabilities {
        server: kind,
        version,
        mode,
        role,
        protocol: conn.protocol(),
        db,
        dbsize,
        used_memory,
        commands: cmds,
        denied: conn.denials().clone(),
        replicas,
        primary,
    })
}

/// `slaveN:ip=...,port=...,state=online,...` lines of `INFO replication`.
pub fn parse_replicas(info: &BTreeMap<String, String>) -> Vec<ReplicaInfo> {
    let mut out: Vec<(usize, ReplicaInfo)> = info
        .iter()
        .filter_map(|(k, v)| {
            let n: usize = k.strip_prefix("slave").or_else(|| k.strip_prefix("replica"))?.parse().ok()?;
            let mut host = None;
            let mut port = None;
            let mut state = String::new();
            for field in v.split(',') {
                match field.split_once('=') {
                    Some(("ip", ip)) => host = Some(ip.to_string()),
                    Some(("port", p)) => port = p.parse().ok(),
                    Some(("state", s)) => state = s.to_string(),
                    _ => {}
                }
            }
            Some((
                n,
                ReplicaInfo {
                    addr: NodeAddr { host: host?, port: port? },
                    state,
                },
            ))
        })
        .collect();
    out.sort_by_key(|(n, _)| *n);
    out.into_iter().map(|(_, r)| r).collect()
}

/// A fresh `DBSIZE`, `None` when denied.
pub async fn dbsize(conn: &mut Conn) -> Option<u64> {
    match conn.raw(&commands::dbsize()).await {
        Ok(Value::Int(n)) if n >= 0 => Some(n as u64),
        _ => None,
    }
}

/// Server identity from `INFO server`. Valkey is recognised by its own
/// version field, never by the inherited `redis_version`.
pub fn identify(info: &BTreeMap<String, String>) -> (ServerKind, String) {
    if let Some(v) = info.get("valkey_version") {
        return (ServerKind::Valkey, v.clone());
    }
    match info.get("server_name").map(|s| s.to_ascii_lowercase()) {
        Some(name) if name == "valkey" => {
            return (ServerKind::Valkey, info.get("redis_version").cloned().unwrap_or_default());
        }
        Some(name) if name != "redis" => {
            let version = info
                .get(&format!("{name}_version"))
                .or_else(|| info.get("redis_version"))
                .cloned()
                .unwrap_or_default();
            return (ServerKind::Other(name), version);
        }
        _ => {}
    }
    match info.get("redis_version") {
        Some(v) => (ServerKind::Redis, v.clone()),
        None => (ServerKind::Other("unknown".into()), String::new()),
    }
}

/// Parse the `key:value` lines of an `INFO` reply.
pub fn parse_info(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Text content of a bulk, simple or verbatim string reply.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        Value::SimpleString(s) => s.clone(),
        Value::VerbatimString { text, .. } => text.clone(),
        Value::Okay => "OK".into(),
        Value::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valkey_is_identified_by_its_own_version_field() {
        let info = parse_info("# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.1.0\r\nredis_mode:standalone\r\n");
        let (kind, version) = identify(&info);
        assert_eq!(kind, ServerKind::Valkey);
        assert_eq!(version, "8.1.0");
    }

    #[test]
    fn redis_is_identified_without_server_name() {
        let info = parse_info("redis_version:7.2.5\r\nredis_mode:standalone");
        assert_eq!(identify(&info), (ServerKind::Redis, "7.2.5".into()));
        let info = parse_info("redis_version:8.2.1\r\nserver_name:redis");
        assert_eq!(identify(&info), (ServerKind::Redis, "8.2.1".into()));
    }

    #[test]
    fn replicas_are_read_from_info_replication() {
        let info = parse_info(
            "# Replication\r\nrole:master\r\nconnected_slaves:2\r\nslave0:ip=10.0.0.5,port=6380,state=online,offset=1,lag=0\r\nslave1:ip=10.0.0.6,port=6381,state=wait_bgsave,offset=0,lag=1\r\n",
        );
        let r = parse_replicas(&info);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].addr.to_string(), "10.0.0.5:6380");
        assert!(r[0].online());
        assert!(!r[1].online());
        assert!(parse_replicas(&parse_info("role:master\r\nconnected_slaves:0")).is_empty());
    }

    #[test]
    fn unknown_servers_are_not_errors() {
        let info = parse_info("server_name:dragonfly\r\ndragonfly_version:1.0\r\nredis_version:7.0");
        assert_eq!(identify(&info), (ServerKind::Other("dragonfly".into()), "1.0".into()));
        assert_eq!(identify(&BTreeMap::new()).0, ServerKind::Other("unknown".into()));
    }
}
