//! Routing (CONCEPT 10.1): one connection per node, opened on first use,
//! and the slot map that says which node a key lives on. A standalone
//! server is a router with one node, so callers never branch on the
//! deployment kind. `MOVED` and `ASK` replies are followed and the slot
//! map corrected.

use std::collections::{BTreeMap, HashMap};

use redis::Value;

use crate::capability::{Capabilities, parse_info, value_text};
use crate::cluster::{self, NodeAddr, Topology};
use crate::commands::{self, write};
use crate::connection::{Conn, ConnectionManager};
use crate::error::{Error, Result};
use crate::notify::config_value;

const MAX_REDIRECTS: usize = 5;

/// The state of one routed command across its attempts.
#[derive(Debug, Clone)]
pub struct Hop {
    key: Option<Vec<u8>>,
    addr: Option<NodeAddr>,
    asking: bool,
    redirects: usize,
    reconnected: bool,
}

impl Hop {
    /// Route by key (its slot's primary).
    pub fn key(key: &[u8]) -> Hop {
        Hop {
            key: Some(key.to_vec()),
            addr: None,
            asking: false,
            redirects: 0,
            reconnected: false,
        }
    }

    /// A fixed node.
    pub fn node(addr: &NodeAddr) -> Hop {
        Hop {
            key: None,
            addr: Some(addr.clone()),
            asking: false,
            redirects: 0,
            reconnected: false,
        }
    }
}

/// Run a command on the node a [`Hop`] resolves to, retrying while
/// [`Router::follow`] says so. `$body` is an expression over `$conn`
/// that yields a `Result`; the macro yields the same `Result`.
///
/// ```ignore
/// let v = routed!(router, Hop::key(key), |c| c.raw(&cmd).await)?;
/// ```
#[macro_export]
macro_rules! routed {
    ($router:expr, $hop:expr, |$conn:ident| $body:expr) => {{
        let mut hop = $hop;
        loop {
            let $conn = match $router.hop(&mut hop).await {
                Ok(c) => c,
                Err(e) => break Err(e),
            };
            let r = $body;
            match r {
                Err(e) if $router.follow(&mut hop, &e).await => continue,
                r => break r,
            }
        }
    }};
}

pub struct Router {
    manager: ConnectionManager,
    seed: NodeAddr,
    topology: Option<Topology>,
    conns: HashMap<NodeAddr, Conn>,
    /// Why the server is browsed as a single node although it reported
    /// cluster mode (`CLUSTER SHARDS` denied, for example).
    note: Option<String>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("seed", &self.seed)
            .field("cluster", &self.topology.is_some())
            .field("open", &self.conns.len())
            .finish()
    }
}

impl Router {
    /// Wrap the connection the capability probe ran on. In cluster mode the
    /// topology is read now; when that is denied the node is browsed alone
    /// and [`Router::note`] says so.
    pub async fn new(manager: ConnectionManager, mut conn: Conn, caps: &Capabilities) -> Router {
        let seed = conn.addr().clone();
        let mut topology = None;
        let mut note = None;
        if caps.is_cluster() {
            if caps.has("cluster|shards") || caps.has("cluster|slots") {
                match cluster::discover(&mut conn, &seed.host).await {
                    Ok(t) => topology = Some(t),
                    Err(e) => note = Some(format!("cluster topology unavailable ({e}); browsing this node only")),
                }
            } else {
                note = Some("CLUSTER SHARDS and CLUSTER SLOTS are denied; browsing this node only".into());
            }
        }
        let mut conns = HashMap::new();
        conns.insert(seed.clone(), conn);
        Router {
            manager,
            seed,
            topology,
            conns,
            note,
        }
    }

    /// A router for a standalone address, without probing.
    pub fn single(manager: ConnectionManager, conn: Conn) -> Router {
        let seed = conn.addr().clone();
        let mut conns = HashMap::new();
        conns.insert(seed.clone(), conn);
        Router {
            manager,
            seed,
            topology: None,
            conns,
            note: None,
        }
    }

    pub fn is_cluster(&self) -> bool {
        self.topology.is_some()
    }

    pub fn topology(&self) -> Option<&Topology> {
        self.topology.as_ref()
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    pub fn seed(&self) -> &NodeAddr {
        &self.seed
    }

    /// Replace the slot map (tests and diagnostics; a wrong map is
    /// corrected by the next `MOVED` reply).
    pub fn set_topology(&mut self, topology: Topology) {
        self.topology = Some(topology);
    }

    pub fn manager(&self) -> &ConnectionManager {
        &self.manager
    }

    /// Re-read the topology from any node that answers and drop
    /// connections to nodes that are gone.
    pub async fn refresh(&mut self) -> Result<()> {
        if self.topology.is_none() {
            return Ok(());
        }
        let mut addrs: Vec<NodeAddr> = vec![self.seed.clone()];
        addrs.extend(self.conns.keys().filter(|a| **a != self.seed).cloned());
        let mut last = None;
        for addr in addrs {
            let seed_host = self.seed.host.clone();
            match self.node(&addr).await {
                Ok(conn) => match cluster::discover(conn, &seed_host).await {
                    Ok(t) => {
                        let known: Vec<NodeAddr> = t.nodes().map(|n| n.addr.clone()).collect();
                        self.conns.retain(|a, _| known.contains(a) || *a == self.seed);
                        self.topology = Some(t);
                        return Ok(());
                    }
                    Err(e) => last = Some(e),
                },
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::Scan("no node answered".into())))
    }

    /// The connection to one node, opened on first use.
    pub async fn node(&mut self, addr: &NodeAddr) -> Result<&mut Conn> {
        if !self.conns.contains_key(addr) {
            let conn = self.manager.connect_to(addr).await?;
            self.conns.insert(addr.clone(), conn);
        }
        Ok(self.conns.get_mut(addr).expect("inserted above"))
    }

    /// Drop the connection to a node (after a transport error).
    pub fn forget(&mut self, addr: &NodeAddr) {
        self.conns.remove(addr);
    }

    /// The primary that serves `key`. Unassigned slots are an error that
    /// a refresh usually resolves.
    pub fn addr_for_key(&self, key: &[u8]) -> Result<NodeAddr> {
        match &self.topology {
            None => Ok(self.seed.clone()),
            Some(t) => {
                let slot = cluster::key_slot(key);
                t.primary_for_slot(slot)
                    .map(|n| n.addr.clone())
                    .ok_or_else(|| Error::Scan(format!("slot {slot} has no primary in the known topology")))
            }
        }
    }

    /// Resolve the connection for one attempt of a routed command: the
    /// primary of the key's slot, or the node a redirect named. Sends
    /// `ASKING` first when the previous attempt was redirected with `ASK`.
    pub async fn hop(&mut self, hop: &mut Hop) -> Result<&mut Conn> {
        if hop.addr.is_none() {
            let key = hop.key.as_deref().ok_or_else(|| Error::Scan("hop without key or node".into()))?;
            hop.addr = Some(match self.addr_for_key(key) {
                Ok(a) => a,
                Err(e) => {
                    self.refresh().await?;
                    self.addr_for_key(key).map_err(|_| e)?
                }
            });
        }
        let addr = hop.addr.clone().expect("set above");
        let asking = std::mem::take(&mut hop.asking);
        let conn = self.node(&addr).await?;
        if asking {
            conn.raw(&commands::asking()).await?;
        }
        Ok(conn)
    }

    /// Decide whether a failed attempt is retried: a `MOVED` corrects the
    /// slot map and refreshes the topology, an `ASK` points the next
    /// attempt at the named node with `ASKING`, a transport error reopens
    /// the connection once. Anything else is final.
    pub async fn follow(&mut self, hop: &mut Hop, e: &Error) -> bool {
        if let Some(r) = e.redirect() {
            if hop.redirects >= MAX_REDIRECTS {
                return false;
            }
            hop.redirects += 1;
            let target = NodeAddr {
                host: r.host,
                port: r.port,
            };
            if r.ask {
                hop.asking = true;
            } else if let Some(t) = &mut self.topology {
                t.moved(r.slot, &target);
                if let Err(e) = self.refresh().await {
                    tracing::debug!("topology refresh after MOVED failed: {e}");
                }
            }
            tracing::debug!(slot = r.slot, ask = r.ask, %target, "following redirect");
            hop.addr = Some(target);
            return true;
        }
        if is_transport(e) && !hop.reconnected {
            hop.reconnected = true;
            if let Some(a) = &hop.addr {
                self.forget(a);
            }
            return true;
        }
        false
    }

    /// Every primary, which for a standalone server is the one node.
    pub fn primaries(&self) -> Vec<NodeAddr> {
        match &self.topology {
            None => vec![self.seed.clone()],
            Some(t) => t.primaries().map(|n| n.addr.clone()).collect(),
        }
    }

    /// Indexes of `keys` grouped by the primary that owns them, in node
    /// order, so pipelines can be issued per node.
    pub fn group_by_node(&self, keys: &[&[u8]]) -> Vec<(NodeAddr, Vec<usize>)> {
        let mut groups: BTreeMap<NodeAddr, Vec<usize>> = BTreeMap::new();
        for (i, k) in keys.iter().enumerate() {
            let addr = self.addr_for_key(k).unwrap_or_else(|_| self.seed.clone());
            groups.entry(addr).or_default().push(i);
        }
        groups.into_iter().collect()
    }

    /// `DBSIZE` summed over the primaries; `None` when every node denied it.
    pub async fn dbsize(&mut self) -> Option<u64> {
        let mut total = None;
        for addr in self.primaries() {
            let n = crate::routed!(self, Hop::node(&addr), |c| c.raw(&commands::dbsize()).await);
            if let Ok(Value::Int(n)) = n
                && n >= 0
            {
                total = Some(total.unwrap_or(0) + n as u64);
            }
        }
        total
    }

    /// The logical databases of the server: how many it is configured
    /// for and how many keys each non-empty one holds (`INFO keyspace`,
    /// summed over the primaries of a cluster).
    pub async fn databases(&mut self) -> Databases {
        let mut keys = BTreeMap::new();
        for addr in self.primaries() {
            let reply = crate::routed!(self, Hop::node(&addr), |c| c.raw(&commands::info("keyspace")).await);
            if let Ok(v) = reply {
                for (db, n) in parse_keyspace(&value_text(&v)) {
                    *keys.entry(db).or_insert(0) += n;
                }
            }
        }
        // A cluster has db 0 only unless the server knows
        // `cluster-databases` (asked, never inferred from a version).
        let cluster = self.is_cluster();
        let parameter = if cluster { "cluster-databases" } else { "databases" };
        let seed = self.seed.clone();
        let reply = crate::routed!(self, Hop::node(&seed), |c| c.raw(&commands::config_get(parameter)).await);
        let configured = reply.ok().and_then(|v| config_value(&v).parse::<u32>().ok()).filter(|n| *n > 0);
        let seen = keys.keys().next_back().map_or(0, |db| *db as u32 + 1);
        let fallback = if cluster { 1 } else { DEFAULT_DATABASES };
        Databases {
            count: configured.unwrap_or(fallback).max(seen),
            configured: configured.is_some(),
            keys,
        }
    }

    /// `FLUSHDB` (or `FLUSHALL`) on every primary.
    pub async fn flush(&mut self, all: bool) -> Result<()> {
        let cmd = if all { write::flushall() } else { write::flushdb() };
        for addr in self.primaries() {
            crate::routed!(self, Hop::node(&addr), |c| c.raw(&cmd).await)?;
        }
        Ok(())
    }
}

/// What a server ships with; assumed when `CONFIG GET databases` is denied.
pub const DEFAULT_DATABASES: u32 = 16;

/// The logical databases a connection can switch between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Databases {
    /// Databases `0..count` exist.
    pub count: u32,
    /// `count` was read from the server's configuration; otherwise it is
    /// the default and a `SELECT` may still be refused.
    pub configured: bool,
    /// Key count of every non-empty database.
    pub keys: BTreeMap<i64, u64>,
}

/// `db3:keys=12,expires=0,avg_ttl=0` lines of `INFO keyspace`.
pub fn parse_keyspace(text: &str) -> Vec<(i64, u64)> {
    parse_info(text)
        .iter()
        .filter_map(|(k, v)| {
            let db = k.strip_prefix("db")?.parse().ok()?;
            let keys = v.split(',').find_map(|f| f.strip_prefix("keys="))?.parse().ok()?;
            Some((db, keys))
        })
        .collect()
}

fn is_transport(e: &Error) -> bool {
    match e {
        Error::Client(e) => matches!(e.kind(), redis::ErrorKind::Io),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_keyspace;

    #[test]
    fn keyspace_section_lists_non_empty_databases() {
        let text = "# Keyspace\r\ndb0:keys=12,expires=3,avg_ttl=0\r\ndb7:keys=1000000,expires=0,avg_ttl=0,subexpiry=0\r\n";
        assert_eq!(parse_keyspace(text), vec![(0, 12), (7, 1_000_000)]);
        assert!(parse_keyspace("# Keyspace\r\n").is_empty());
    }
}
