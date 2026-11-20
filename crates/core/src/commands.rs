//! Every command the backend sends to a server is built here, as a raw
//! [`redis::Cmd`]. Nothing else in the crate calls `redis::cmd` directly, so
//! `grep -n 'cmd("' crates/core/src/commands.rs` is the complete list for an
//! operator writing an ACL. The same list is kept in `docs/commands.md` and a
//! test keeps the two in sync.
//!
//! `HELLO`, `AUTH`, `SELECT` and `CLIENT SETINFO` are sent by the client crate
//! during the connection handshake and are listed in the document as well.
//!
//! One builder is not on that list: [`console::user`] sends what the user
//! typed into the command console. Those are the user's commands, run under
//! the user's ACL, and no rule for the browser can name them in advance.

use redis::{Cmd, Pipeline};

/// `PING`: round-trip baseline for the adaptive scan.
pub fn ping() -> Cmd {
    let mut c = redis::cmd("PING");
    c.arg("keyspace");
    c
}

/// `INFO <section>`: server identity, replication role, memory.
pub fn info(section: &str) -> Cmd {
    let mut c = redis::cmd("INFO");
    c.arg(section);
    c
}

/// `DBSIZE`: exact key count for scan progress.
pub fn dbsize() -> Cmd {
    redis::cmd("DBSIZE")
}

/// `COMMAND INFO name [name ...]`: probe which commands the server knows.
/// Subcommands are written in pipe notation, for example `memory|usage`.
pub fn command_info(names: &[&str]) -> Cmd {
    let mut c = redis::cmd("COMMAND");
    c.arg("INFO");
    for n in names {
        c.arg(*n);
    }
    c
}

/// `COMMAND GETKEYS command [arg ...]`: which arguments of a command the
/// user typed are keys, so the console can route it on a cluster and read
/// those keys again after a write.
pub fn command_getkeys(args: &[Vec<u8>]) -> Cmd {
    let mut c = redis::cmd("COMMAND");
    c.arg("GETKEYS");
    for a in args {
        c.arg(a);
    }
    c
}

/// The command console (CONCEPT 8): the one place where a command comes
/// from outside this file.
pub mod console {
    use redis::Cmd;

    /// Whatever the user typed, argument by argument, as it was typed.
    /// Never used by the browser itself; `crate::console` decides first
    /// what may run and what needs a confirmation.
    pub fn user(args: &[Vec<u8>]) -> Cmd {
        let mut c = Cmd::new();
        for a in args {
            c.arg(a);
        }
        c
    }
}

/// `SCAN cursor [MATCH pattern] COUNT n [TYPE type]`: the only enumeration
/// primitive the browser ever uses.
pub fn scan(cursor: u64, pattern: Option<&[u8]>, count: usize, type_filter: Option<&str>) -> Cmd {
    let mut c = redis::cmd("SCAN");
    c.cursor_arg(cursor);
    if let Some(p) = pattern {
        c.arg("MATCH").arg(p);
    }
    c.arg("COUNT").arg(count);
    if let Some(t) = type_filter {
        c.arg("TYPE").arg(t);
    }
    c
}

/// `TYPE key`: lazy type resolution for visible rows (pipelined by callers).
pub fn key_type(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("TYPE");
    c.arg(key);
    c
}

/// `TTL key`: lazy TTL for visible rows (pipelined by callers).
pub fn ttl(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("TTL");
    c.arg(key);
    c
}

/// `MEMORY USAGE key SAMPLES 0`: lazy memory estimate for visible rows.
pub fn memory_usage(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("MEMORY");
    c.arg("USAGE").arg(key).arg("SAMPLES").arg(0);
    c
}

/// `EXISTS key [key ...]`: verify-on-open and index reconciliation.
pub fn exists(keys: &[&[u8]]) -> Cmd {
    let mut c = redis::cmd("EXISTS");
    for k in keys {
        c.arg(*k);
    }
    c
}

/// `OBJECT ENCODING key`: shown next to the type when a key is opened.
pub fn object_encoding(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("OBJECT");
    c.arg("ENCODING").arg(key);
    c
}

/// `STRLEN key`
pub fn strlen(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("STRLEN");
    c.arg(key);
    c
}

/// `GETRANGE key start end`: string previews and chunks.
pub fn getrange(key: &[u8], start: i64, end: i64) -> Cmd {
    let mut c = redis::cmd("GETRANGE");
    c.arg(key).arg(start).arg(end);
    c
}

/// `HLEN key`
pub fn hlen(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("HLEN");
    c.arg(key);
    c
}

/// `HSCAN key cursor [MATCH pattern] COUNT n`
pub fn hscan(key: &[u8], cursor: u64, pattern: Option<&[u8]>, count: usize) -> Cmd {
    let mut c = redis::cmd("HSCAN");
    c.arg(key).cursor_arg(cursor);
    if let Some(p) = pattern {
        c.arg("MATCH").arg(p);
    }
    c.arg("COUNT").arg(count);
    c
}

/// `SCARD key`
pub fn scard(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("SCARD");
    c.arg(key);
    c
}

/// `SSCAN key cursor [MATCH pattern] COUNT n`
pub fn sscan(key: &[u8], cursor: u64, pattern: Option<&[u8]>, count: usize) -> Cmd {
    let mut c = redis::cmd("SSCAN");
    c.arg(key).cursor_arg(cursor);
    if let Some(p) = pattern {
        c.arg("MATCH").arg(p);
    }
    c.arg("COUNT").arg(count);
    c
}

/// `ZCARD key`
pub fn zcard(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("ZCARD");
    c.arg(key);
    c
}

/// `ZSCAN key cursor [MATCH pattern] COUNT n`
pub fn zscan(key: &[u8], cursor: u64, pattern: Option<&[u8]>, count: usize) -> Cmd {
    let mut c = redis::cmd("ZSCAN");
    c.arg(key).cursor_arg(cursor);
    if let Some(p) = pattern {
        c.arg("MATCH").arg(p);
    }
    c.arg("COUNT").arg(count);
    c
}

/// `ZRANGE key start stop WITHSCORES`: rank windows of a sorted set.
pub fn zrange(key: &[u8], start: i64, stop: i64) -> Cmd {
    let mut c = redis::cmd("ZRANGE");
    c.arg(key).arg(start).arg(stop).arg("WITHSCORES");
    c
}

/// `LLEN key`
pub fn llen(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("LLEN");
    c.arg(key);
    c
}

/// `LRANGE key start stop`: list windows.
pub fn lrange(key: &[u8], start: i64, stop: i64) -> Cmd {
    let mut c = redis::cmd("LRANGE");
    c.arg(key).arg(start).arg(stop);
    c
}

/// `XLEN key`
pub fn xlen(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("XLEN");
    c.arg(key);
    c
}

/// `XRANGE key start + COUNT n`: stream pages; `start` is `-` or an
/// exclusive `(id`.
pub fn xrange(key: &[u8], start: &str, count: usize) -> Cmd {
    let mut c = redis::cmd("XRANGE");
    c.arg(key).arg(start).arg("+").arg("COUNT").arg(count);
    c
}

/// `DUMP key`: size and a raw peek for types without a viewer.
pub fn dump(key: &[u8]) -> Cmd {
    let mut c = redis::cmd("DUMP");
    c.arg(key);
    c
}

/// Write commands (CONCEPT section 7, write-through; section 8, safety).
/// Every mutation the browser performs is one of these, so an operator can
/// grant exactly the writes they allow. Pattern deletes are batched
/// `UNLINK` calls; `DEL` is the fallback when `UNLINK` is missing.
pub mod write {
    use super::*;

    /// `UNLINK key [key ...]`: asynchronous delete, the default.
    pub fn unlink(keys: &[&[u8]]) -> Cmd {
        let mut c = redis::cmd("UNLINK");
        for k in keys {
            c.arg(*k);
        }
        c
    }

    /// `DEL key [key ...]`: when `UNLINK` is unavailable; also the test suite.
    pub fn del(keys: &[&[u8]]) -> Cmd {
        let mut c = redis::cmd("DEL");
        for k in keys {
            c.arg(*k);
        }
        c
    }

    /// `RENAME key newkey`
    pub fn rename(key: &[u8], to: &[u8]) -> Cmd {
        let mut c = redis::cmd("RENAME");
        c.arg(key).arg(to);
        c
    }

    /// `EXPIRE key seconds`
    pub fn expire(key: &[u8], seconds: i64) -> Cmd {
        let mut c = redis::cmd("EXPIRE");
        c.arg(key).arg(seconds);
        c
    }

    /// `PERSIST key`
    pub fn persist(key: &[u8]) -> Cmd {
        let mut c = redis::cmd("PERSIST");
        c.arg(key);
        c
    }

    /// `SET key value [KEEPTTL]`: string edits keep the key's expiry; the
    /// generator writes test data with it too.
    pub fn set(key: &[u8], value: &[u8], keep_ttl: bool) -> Cmd {
        let mut c = redis::cmd("SET");
        c.arg(key).arg(value);
        if keep_ttl {
            c.arg("KEEPTTL");
        }
        c
    }

    /// `HSET key field value`
    pub fn hset(key: &[u8], field: &[u8], value: &[u8]) -> Cmd {
        let mut c = redis::cmd("HSET");
        c.arg(key).arg(field).arg(value);
        c
    }

    /// `HDEL key field`
    pub fn hdel(key: &[u8], field: &[u8]) -> Cmd {
        let mut c = redis::cmd("HDEL");
        c.arg(key).arg(field);
        c
    }

    /// `SADD key member`
    pub fn sadd(key: &[u8], member: &[u8]) -> Cmd {
        let mut c = redis::cmd("SADD");
        c.arg(key).arg(member);
        c
    }

    /// `SREM key member`
    pub fn srem(key: &[u8], member: &[u8]) -> Cmd {
        let mut c = redis::cmd("SREM");
        c.arg(key).arg(member);
        c
    }

    /// `ZADD key score member`
    pub fn zadd(key: &[u8], score: f64, member: &[u8]) -> Cmd {
        let mut c = redis::cmd("ZADD");
        c.arg(key).arg(score).arg(member);
        c
    }

    /// `ZREM key member`
    pub fn zrem(key: &[u8], member: &[u8]) -> Cmd {
        let mut c = redis::cmd("ZREM");
        c.arg(key).arg(member);
        c
    }

    /// `LPUSH key value`
    pub fn lpush(key: &[u8], value: &[u8]) -> Cmd {
        let mut c = redis::cmd("LPUSH");
        c.arg(key).arg(value);
        c
    }

    /// `RPUSH key value`
    pub fn rpush(key: &[u8], value: &[u8]) -> Cmd {
        let mut c = redis::cmd("RPUSH");
        c.arg(key).arg(value);
        c
    }

    /// `LSET key index value`
    pub fn lset(key: &[u8], index: i64, value: &[u8]) -> Cmd {
        let mut c = redis::cmd("LSET");
        c.arg(key).arg(index).arg(value);
        c
    }

    /// `LREM key count value`: removing a list item by index is `LSET` to
    /// a sentinel followed by `LREM key 1 sentinel`.
    pub fn lrem(key: &[u8], count: i64, value: &[u8]) -> Cmd {
        let mut c = redis::cmd("LREM");
        c.arg(key).arg(count).arg(value);
        c
    }

    /// `XADD key * field value [field value ...]`
    pub fn xadd(key: &[u8], fields: &[(Vec<u8>, Vec<u8>)]) -> Cmd {
        let mut c = redis::cmd("XADD");
        c.arg(key).arg("*");
        for (f, v) in fields {
            c.arg(f).arg(v);
        }
        c
    }

    /// `XDEL key id`
    pub fn xdel(key: &[u8], id: &str) -> Cmd {
        let mut c = redis::cmd("XDEL");
        c.arg(key).arg(id);
        c
    }

    /// `FLUSHDB`: only after the user typed the connection's name (CONCEPT 8).
    pub fn flushdb() -> Cmd {
        redis::cmd("FLUSHDB")
    }

    /// `FLUSHALL`: same confirmation, every logical database.
    pub fn flushall() -> Cmd {
        redis::cmd("FLUSHALL")
    }
}

/// Commands used only by the test-data generator and the test suite, never
/// by the browser; the write commands they share are the browser's own.
pub mod testing {
    use super::*;

    /// `SET key value`
    pub fn set(key: &[u8], value: &[u8]) -> Cmd {
        write::set(key, value, false)
    }

    /// `DEL key [key ...]`
    pub fn del(keys: &[&[u8]]) -> Cmd {
        write::del(keys)
    }

    /// `FLUSHDB`: only on explicit request of the generator's `--flush` flag.
    pub fn flushdb() -> Cmd {
        write::flushdb()
    }

    /// `SELECT db`
    /// `CLUSTER KEYSLOT key`: checks the client-side slot computation.
    /// `ACL SETUSER name rule [rule ...]`: a restricted user for the tests
    /// of denied commands.
    pub fn acl_setuser(name: &str, rules: &[&str]) -> Cmd {
        let mut c = redis::cmd("ACL");
        c.arg("SETUSER").arg(name);
        for r in rules {
            c.arg(*r);
        }
        c
    }

    /// `ACL DELUSER name`
    pub fn acl_deluser(name: &str) -> Cmd {
        let mut c = redis::cmd("ACL");
        c.arg("DELUSER").arg(name);
        c
    }

    pub fn cluster_keyslot(key: &[u8]) -> Cmd {
        let mut c = redis::cmd("CLUSTER");
        c.arg("KEYSLOT").arg(key);
        c
    }

    pub fn select(db: i64) -> Cmd {
        let mut c = redis::cmd("SELECT");
        c.arg(db);
        c
    }
}

/// `CLUSTER SHARDS`: topology with slot ranges, roles and health (Redis 7+,
/// Valkey). The backend routes by slot itself (CONCEPT 10.1).
pub fn cluster_shards() -> Cmd {
    let mut c = redis::cmd("CLUSTER");
    c.arg("SHARDS");
    c
}

/// `CLUSTER SLOTS`: the older topology reply, used when `CLUSTER SHARDS`
/// is missing or denied.
pub fn cluster_slots() -> Cmd {
    let mut c = redis::cmd("CLUSTER");
    c.arg("SLOTS");
    c
}

/// `READONLY`: lets a cluster replica serve `SCAN` and reads.
pub fn readonly() -> Cmd {
    redis::cmd("READONLY")
}

/// `ASKING`: precedes the retried command after an `ASK` redirect.
pub fn asking() -> Cmd {
    redis::cmd("ASKING")
}

/// `CONFIG GET parameter`: whether keyspace notifications are on.
pub fn config_get(parameter: &str) -> Cmd {
    let mut c = redis::cmd("CONFIG");
    c.arg("GET").arg(parameter);
    c
}

/// `CONFIG SET parameter value`: only to enable keyspace notifications,
/// and only after the user asked for it.
pub fn config_set(parameter: &str, value: &str) -> Cmd {
    let mut c = redis::cmd("CONFIG");
    c.arg("SET").arg(parameter).arg(value);
    c
}

/// `SLOWLOG LEN`: the normal scan profile backs off when it grows.
pub fn slowlog_len() -> Cmd {
    let mut c = redis::cmd("SLOWLOG");
    c.arg("LEN");
    c
}

/// A pipeline of independent commands; replies come back in order.
pub fn pipeline() -> Pipeline {
    redis::pipe()
}

#[cfg(test)]
mod tests {
    /// `docs/commands.md` must list every command this file builds.
    #[test]
    fn documented_commands_match_source() {
        let source = include_str!("commands.rs");
        let doc = include_str!("../../../docs/commands.md");
        let mut missing = Vec::new();
        for line in source.lines() {
            if let Some(rest) = line.trim().strip_prefix("redis::cmd(\"") {
                let name = rest.split('"').next().unwrap();
                if !doc.contains(&format!("`{name}")) {
                    missing.push(name.to_string());
                }
            }
        }
        assert!(missing.is_empty(), "undocumented commands: {missing:?}");
    }

    /// The console's builder takes its command from the user, so the scan
    /// above cannot see it; the document must say that it exists.
    #[test]
    fn the_console_is_documented_as_the_users_commands() {
        let source = include_str!("commands.rs");
        let doc = include_str!("../../../docs/commands.md");
        assert_eq!(
            source.matches("Cmd::new()").count(),
            2,
            "one builder (and this test) may take a command from outside"
        );
        assert!(doc.contains("## Console"), "docs/commands.md needs its Console section");
    }
}
