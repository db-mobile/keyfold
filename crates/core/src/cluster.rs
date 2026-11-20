//! Cluster topology (CONCEPT 10.1): the backend reads `CLUSTER SHARDS`
//! (or `CLUSTER SLOTS`), remembers which node serves which slot range and
//! routes by slot itself. The client crate's cluster abstraction is not
//! used. Nothing here depends on the server being Redis or Valkey; both
//! answer these commands the same way.

use std::collections::BTreeMap;
use std::fmt;

use redis::Value;

use crate::capability::value_text;
use crate::commands;
use crate::connection::Conn;
use crate::error::{Error, Result};

pub const SLOTS: usize = 16384;

/// `CRC16-XMODEM` table, the polynomial cluster hashing uses.
const CRC16: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc = (crc << 8) ^ CRC16[((crc >> 8) as u8 ^ b) as usize];
    }
    crc
}

/// The hash slot of a key: `CRC16` of the key, or of the first non-empty
/// `{hashtag}` when there is one, modulo 16384.
pub fn key_slot(key: &[u8]) -> u16 {
    let hashed = match key.iter().position(|&b| b == b'{') {
        Some(open) => match key[open + 1..].iter().position(|&b| b == b'}') {
            Some(0) | None => key,
            Some(len) => &key[open + 1..open + 1 + len],
        },
        None => key,
    };
    crc16(hashed) % SLOTS as u16
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeAddr {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for NodeAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Primary,
    Replica,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Primary => "primary",
            Role::Replica => "replica",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub addr: NodeAddr,
    pub role: Role,
    /// `health` from `CLUSTER SHARDS` is `online`; `CLUSTER SLOTS` lists
    /// only nodes it considers serving.
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    /// Inclusive slot ranges.
    pub slots: Vec<(u16, u16)>,
    pub nodes: Vec<Node>,
}

impl Shard {
    pub fn primary(&self) -> Option<&Node> {
        self.nodes.iter().find(|n| n.role == Role::Primary)
    }

    pub fn replicas(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.role == Role::Replica)
    }

    pub fn slot_count(&self) -> usize {
        self.slots
            .iter()
            .map(|(lo, hi)| (*hi as usize + 1).saturating_sub(*lo as usize))
            .sum()
    }
}

/// Which shard serves each slot. Shards without a primary are kept (their
/// slots are unroutable until the topology is refreshed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    pub shards: Vec<Shard>,
    slot_map: Vec<u16>,
}

impl Topology {
    pub fn new(shards: Vec<Shard>) -> Topology {
        let mut slot_map = vec![u16::MAX; SLOTS];
        for (i, s) in shards.iter().enumerate() {
            for &(lo, hi) in &s.slots {
                for slot in lo..=hi.min(SLOTS as u16 - 1) {
                    slot_map[slot as usize] = i as u16;
                }
            }
        }
        Topology { shards, slot_map }
    }

    pub fn shard_for_slot(&self, slot: u16) -> Option<&Shard> {
        let i = *self.slot_map.get(slot as usize)?;
        (i != u16::MAX).then(|| &self.shards[i as usize])
    }

    pub fn primary_for_slot(&self, slot: u16) -> Option<&Node> {
        self.shard_for_slot(slot).and_then(Shard::primary)
    }

    pub fn primary_for_key(&self, key: &[u8]) -> Option<&Node> {
        self.primary_for_slot(key_slot(key))
    }

    pub fn primaries(&self) -> impl Iterator<Item = &Node> {
        self.shards.iter().filter_map(Shard::primary)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.shards.iter().flat_map(|s| s.nodes.iter())
    }

    pub fn node_count(&self) -> usize {
        self.nodes().count()
    }

    /// Slots no shard claims; a healthy cluster has none.
    pub fn unassigned_slots(&self) -> usize {
        self.slot_map.iter().filter(|&&i| i == u16::MAX).count()
    }

    /// Point one slot at the node a `MOVED` reply named, without a full
    /// refresh. Unknown nodes get a shard of their own until the next
    /// discovery fills in the rest.
    pub fn moved(&mut self, slot: u16, addr: &NodeAddr) {
        let target = self.shards.iter().position(|s| s.primary().is_some_and(|p| &p.addr == addr));
        let idx = match target {
            Some(i) => i,
            None => {
                self.shards.push(Shard {
                    slots: Vec::new(),
                    nodes: vec![Node {
                        id: String::new(),
                        addr: addr.clone(),
                        role: Role::Primary,
                        online: true,
                    }],
                });
                self.shards.len() - 1
            }
        };
        if let Some(prev) = self.slot_map.get(slot as usize).copied()
            && prev != u16::MAX
            && prev as usize != idx
        {
            let s = &mut self.shards[prev as usize];
            s.slots = s
                .slots
                .iter()
                .flat_map(|&(lo, hi)| {
                    let mut out = Vec::new();
                    if slot < lo || slot > hi {
                        out.push((lo, hi));
                    } else {
                        if lo < slot {
                            out.push((lo, slot - 1));
                        }
                        if slot < hi {
                            out.push((slot + 1, hi));
                        }
                    }
                    out
                })
                .collect();
        }
        self.shards[idx].slots.push((slot, slot));
        self.slot_map[slot as usize] = idx as u16;
    }
}

/// Read the topology from a connected node. `CLUSTER SHARDS` first; when it
/// is missing or denied, `CLUSTER SLOTS`. `seed_host` replaces an endpoint
/// the server does not know (`?` or empty), which happens on single-host
/// setups that never set `cluster-announce-ip`.
pub async fn discover(conn: &mut Conn, seed_host: &str) -> Result<Topology> {
    match conn.raw(&commands::cluster_shards()).await {
        Ok(v) => parse_shards(&v, seed_host),
        Err(e) if e.is_denied() || is_unknown_subcommand(&e) => {
            let v = conn.raw(&commands::cluster_slots()).await?;
            parse_slots(&v, seed_host)
        }
        Err(e) => Err(e),
    }
}

fn is_unknown_subcommand(e: &Error) -> bool {
    match e {
        Error::Server { detail, .. } => {
            let d = detail.to_ascii_lowercase();
            d.contains("unknown subcommand") || d.contains("unknown command")
        }
        _ => false,
    }
}

/// Key/value pairs of a map reply, in RESP3 (`Map`) or RESP2 (flat array).
fn pairs(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Map(items) => items.iter().map(|(k, v)| (value_text(k), v.clone())).collect(),
        Value::Array(items) => items
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| (value_text(&c[0]), c[1].clone()))
            .collect(),
        _ => Vec::new(),
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::BulkString(b) => std::str::from_utf8(b).ok()?.parse().ok(),
        Value::SimpleString(s) => s.parse().ok(),
        _ => None,
    }
}

fn host_or_seed(text: &str, seed_host: &str) -> String {
    if text.is_empty() || text == "?" {
        seed_host.to_string()
    } else {
        text.to_string()
    }
}

/// `CLUSTER SHARDS`: an array of shards, each a map with `slots` (a flat
/// list of inclusive start/end pairs) and `nodes` (maps with `id`,
/// `endpoint`/`ip`, `port` or `tls-port`, `role`, `health`).
pub fn parse_shards(v: &Value, seed_host: &str) -> Result<Topology> {
    let Value::Array(shards) = v else {
        return Err(Error::reply("CLUSTER SHARDS", format!("expected an array, got {v:?}")));
    };
    let mut out = Vec::with_capacity(shards.len());
    for shard in shards {
        let fields: BTreeMap<String, Value> = pairs(shard).into_iter().collect();
        let slots = match fields.get("slots") {
            Some(Value::Array(items)) => items
                .chunks(2)
                .filter_map(|c| match (c.first().and_then(int), c.get(1).and_then(int)) {
                    (Some(lo), Some(hi)) if (0..SLOTS as i64).contains(&lo) && (0..SLOTS as i64).contains(&hi) => {
                        Some((lo as u16, hi as u16))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let nodes = match fields.get("nodes") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|n| {
                    let f: BTreeMap<String, Value> = pairs(n).into_iter().collect();
                    let port = f.get("port").and_then(int).or_else(|| f.get("tls-port").and_then(int))?;
                    let host = f
                        .get("endpoint")
                        .map(value_text)
                        .filter(|s| !s.is_empty() && s != "?")
                        .or_else(|| f.get("ip").map(value_text))
                        .unwrap_or_default();
                    let role = match f.get("role").map(value_text).as_deref() {
                        Some("master") | Some("primary") => Role::Primary,
                        _ => Role::Replica,
                    };
                    let online = f.get("health").map(value_text).is_none_or(|h| h == "online");
                    Some(Node {
                        id: f.get("id").map(value_text).unwrap_or_default(),
                        addr: NodeAddr {
                            host: host_or_seed(&host, seed_host),
                            port: port as u16,
                        },
                        role,
                        online,
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        out.push(Shard { slots, nodes });
    }
    Ok(Topology::new(out))
}

/// `CLUSTER SLOTS`: an array of `[start, end, primary, replica...]` where
/// each node is `[ip, port, id, ...]`. Ranges of the same primary are one
/// shard.
pub fn parse_slots(v: &Value, seed_host: &str) -> Result<Topology> {
    let Value::Array(ranges) = v else {
        return Err(Error::reply("CLUSTER SLOTS", format!("expected an array, got {v:?}")));
    };
    let mut by_primary: BTreeMap<NodeAddr, Shard> = BTreeMap::new();
    for range in ranges {
        let Value::Array(items) = range else { continue };
        let (Some(lo), Some(hi)) = (items.first().and_then(int), items.get(1).and_then(int)) else {
            continue;
        };
        if !(0..SLOTS as i64).contains(&lo) || !(0..SLOTS as i64).contains(&hi) {
            continue;
        }
        let mut nodes = Vec::new();
        for (i, n) in items.iter().skip(2).enumerate() {
            let Value::Array(parts) = n else { continue };
            let host = parts.first().map(value_text).unwrap_or_default();
            let Some(port) = parts.get(1).and_then(int) else { continue };
            nodes.push(Node {
                id: parts.get(2).map(value_text).unwrap_or_default(),
                addr: NodeAddr {
                    host: host_or_seed(&host, seed_host),
                    port: port as u16,
                },
                role: if i == 0 { Role::Primary } else { Role::Replica },
                online: true,
            });
        }
        let Some(primary) = nodes.first().cloned() else { continue };
        let shard = by_primary.entry(primary.addr).or_insert(Shard { slots: Vec::new(), nodes });
        shard.slots.push((lo as u16, hi as u16));
    }
    Ok(Topology::new(by_primary.into_values().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn slots_match_the_reference_values() {
        // values from the cluster specification and `CLUSTER KEYSLOT`
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(key_slot(b"foo"), 12182);
        assert_eq!(key_slot(b"bar"), 5061);
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"user1000"));
        assert_eq!(key_slot(b"{user1000}.followers"), key_slot(b"user1000"));
        // an empty hashtag means the whole key is hashed
        assert_eq!(key_slot(b"foo{}{bar}"), crc16(b"foo{}{bar}") % 16384);
        // only the first opening brace counts
        assert_eq!(key_slot(b"foo{{bar}}zap"), key_slot(b"{bar"));
        assert_eq!(key_slot(b"foo{bar}{zap}"), key_slot(b"bar"));
        // an unclosed brace hashes the whole key
        assert_eq!(key_slot(b"foo{bar"), crc16(b"foo{bar") % 16384);
    }

    #[test]
    fn shards_reply_is_parsed_in_both_protocols() {
        let node = |id: &str, port: i64, role: &str, health: &str| {
            Value::Map(vec![
                (bulk("id"), bulk(id)),
                (bulk("port"), Value::Int(port)),
                (bulk("ip"), bulk("10.0.0.1")),
                (bulk("endpoint"), bulk("?")),
                (bulk("role"), bulk(role)),
                (bulk("replication-offset"), Value::Int(0)),
                (bulk("health"), bulk(health)),
            ])
        };
        let shards = Value::Array(vec![
            Value::Map(vec![
                (bulk("slots"), Value::Array(vec![Value::Int(0), Value::Int(5460)])),
                (
                    bulk("nodes"),
                    Value::Array(vec![node("a", 7000, "master", "online"), node("a2", 7003, "replica", "loading")]),
                ),
            ]),
            Value::Map(vec![
                (
                    bulk("slots"),
                    Value::Array(vec![Value::Int(5461), Value::Int(10922), Value::Int(16000), Value::Int(16383)]),
                ),
                (bulk("nodes"), Value::Array(vec![node("b", 7001, "master", "online")])),
            ]),
        ]);
        let t = parse_shards(&shards, "seed.local").unwrap();
        assert_eq!(t.shards.len(), 2);
        assert_eq!(t.shards[0].primary().unwrap().addr.to_string(), "10.0.0.1:7000");
        assert!(!t.shards[0].replicas().next().unwrap().online);
        assert_eq!(t.shards[1].slot_count(), 5462 + 384);
        assert_eq!(t.primary_for_slot(16001).unwrap().id, "b");
        assert_eq!(t.primary_for_slot(100).unwrap().id, "a");
        assert_eq!(t.unassigned_slots(), 16384 - 5461 - 5462 - 384);

        // RESP2: maps come as flat arrays, endpoints may be unknown
        let flat = Value::Array(vec![Value::Array(vec![
            bulk("slots"),
            Value::Array(vec![bulk("0"), bulk("16383")]),
            bulk("nodes"),
            Value::Array(vec![Value::Array(vec![
                bulk("id"),
                bulk("x"),
                bulk("port"),
                bulk("6379"),
                bulk("endpoint"),
                bulk("?"),
                bulk("ip"),
                bulk("?"),
                bulk("role"),
                bulk("master"),
                bulk("health"),
                bulk("online"),
            ])]),
        ])]);
        let t = parse_slots_or_shards(&flat);
        assert_eq!(t.primary_for_key(b"anything").unwrap().addr.to_string(), "seed.local:6379");
    }

    fn parse_slots_or_shards(v: &Value) -> Topology {
        parse_shards(v, "seed.local").unwrap()
    }

    #[test]
    fn slots_reply_groups_ranges_by_primary() {
        let node = |host: &str, port: i64, id: &str| Value::Array(vec![bulk(host), Value::Int(port), bulk(id)]);
        let v = Value::Array(vec![
            Value::Array(vec![Value::Int(0), Value::Int(100), node("", 7000, "a"), node("h", 7003, "a2")]),
            Value::Array(vec![Value::Int(200), Value::Int(300), node("", 7000, "a"), node("h", 7003, "a2")]),
            Value::Array(vec![Value::Int(101), Value::Int(199), node("h", 7001, "b")]),
        ]);
        let t = parse_slots(&v, "seed").unwrap();
        assert_eq!(t.shards.len(), 2);
        let a = t.primary_for_slot(250).unwrap();
        assert_eq!(a.addr.to_string(), "seed:7000");
        assert_eq!(t.shard_for_slot(250).unwrap().slots.len(), 2);
        assert_eq!(t.shard_for_slot(250).unwrap().replicas().count(), 1);
        assert_eq!(t.primary_for_slot(150).unwrap().id, "b");
        assert!(t.primary_for_slot(301).is_none());
    }

    #[test]
    fn moved_repoints_one_slot() {
        let v = Value::Array(vec![
            Value::Array(vec![
                Value::Int(0),
                Value::Int(10),
                Value::Array(vec![bulk("h"), Value::Int(1), bulk("a")]),
            ]),
            Value::Array(vec![
                Value::Int(11),
                Value::Int(20),
                Value::Array(vec![bulk("h"), Value::Int(2), bulk("b")]),
            ]),
        ]);
        let mut t = parse_slots(&v, "seed").unwrap();
        let b = NodeAddr { host: "h".into(), port: 2 };
        t.moved(5, &b);
        assert_eq!(t.primary_for_slot(5).unwrap().id, "b");
        assert_eq!(t.primary_for_slot(4).unwrap().id, "a");
        assert_eq!(t.primary_for_slot(6).unwrap().id, "a");
        assert_eq!(t.shard_for_slot(4).unwrap().slots, vec![(0, 4), (6, 10)]);
        // an unknown node gets a shard of its own
        let c = NodeAddr { host: "h".into(), port: 3 };
        t.moved(15, &c);
        assert_eq!(t.primary_for_slot(15).unwrap().addr, c);
        assert_eq!(t.shards.len(), 3);
    }
}
