//! Values (CONCEPT section 6): one pipeline on selection, paged reads for
//! collections, previews and chunks for strings, and a raw fallback for
//! types without a viewer. Nothing here is ever called on hover.

use redis::Value;

use crate::capability::value_text;
use crate::commands;
use crate::connection::Conn;
use crate::error::{Error, Result};

/// Fields per page and bytes per string chunk.
pub const PAGE: usize = 200;
pub const PREVIEW_BYTES: i64 = 65_536;
pub const CHUNK_BYTES: i64 = 1 << 20;

/// What one selection learns in a single round trip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyInfo {
    pub exists: bool,
    /// The exact `TYPE` string; the type registry dispatches on it.
    pub key_type: String,
    pub ttl: Option<i64>,
    pub mem: Option<i64>,
    pub encoding: Option<String>,
    /// Bytes for strings, fields/members/items/entries for collections.
    pub length: Option<i64>,
}

/// Inspect a key: `TYPE`, `TTL`, `MEMORY USAGE`, `OBJECT ENCODING` and the
/// length commands of every native type, in one pipeline with errors
/// ignored, so the right length comes back without a second round trip.
pub async fn inspect(conn: &mut Conn, key: &[u8]) -> Result<KeyInfo> {
    let mut pipe = commands::pipeline();
    pipe.add_command(commands::key_type(key));
    pipe.add_command(commands::ttl(key));
    pipe.add_command(commands::memory_usage(key));
    pipe.add_command(commands::object_encoding(key));
    pipe.add_command(commands::strlen(key));
    pipe.add_command(commands::hlen(key));
    pipe.add_command(commands::scard(key));
    pipe.add_command(commands::zcard(key));
    pipe.add_command(commands::llen(key));
    pipe.add_command(commands::xlen(key));
    pipe.ignore_errors();
    let r = conn.pipeline(&pipe).await?;
    let int = |i: usize| match r.get(i) {
        Some(Ok(Value::Int(n))) => Some(*n),
        _ => None,
    };
    let text = |i: usize| match r.get(i) {
        Some(Ok(v)) if !matches!(v, Value::Nil) => Some(value_text(v)),
        _ => None,
    };
    let key_type = text(0).unwrap_or_else(|| "none".into());
    let exists = key_type != "none";
    let length = match key_type.as_str() {
        "string" => int(4),
        "hash" => int(5),
        "set" => int(6),
        "zset" => int(7),
        "list" => int(8),
        "stream" => int(9),
        _ => None,
    };
    Ok(KeyInfo {
        exists,
        key_type: if exists { key_type } else { "none".into() },
        ttl: int(1).filter(|&t| t >= 0 && exists),
        mem: int(2).filter(|&m| m >= 0 && exists),
        encoding: if exists { text(3) } else { None },
        length,
    })
}

/// One field or member of a collection.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub field: Vec<u8>,
    /// Hash value, zset score as text, stream fields joined; empty for sets
    /// and list items (the item is `field`).
    pub value: Vec<u8>,
    /// Sorted set score, when applicable.
    pub score: Option<f64>,
}

/// A page of a collection and how to get the next one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ValuePage {
    pub entries: Vec<Entry>,
    /// `SCAN`-style cursor for hashes, sets, zsets; `0` when done.
    pub cursor: u64,
    /// Next rank or index for `ZRANGE`/`LRANGE`; stream id for `XRANGE`.
    pub next: Option<String>,
    pub done: bool,
}

/// Where a page starts.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Cursor {
    #[default]
    Start,
    /// Server cursor from the previous `HSCAN`/`SSCAN`/`ZSCAN`.
    Scan(u64),
    /// Rank or index of the next item (`ZRANGE`, `LRANGE`).
    Rank(i64),
    /// Last stream id seen; the next page is exclusive of it.
    StreamId(String),
}

/// Read one page of a collection. Hashes, sets and sorted sets use their
/// `SCAN` family (with `MATCH` for `filter`); sorted sets can also be read
/// by rank with scores, lists by index, streams by id.
pub async fn page(conn: &mut Conn, key: &[u8], key_type: &str, cursor: Cursor, limit: usize, filter: Option<&[u8]>) -> Result<ValuePage> {
    let limit = limit.clamp(1, PAGE);
    match (key_type, cursor) {
        ("hash", c) => scan_page(conn, commands::hscan(key, scan_cursor(c)?, filter, limit), true).await,
        ("set", c) => scan_page(conn, commands::sscan(key, scan_cursor(c)?, filter, limit), false).await,
        ("zset", Cursor::Rank(r)) => {
            let v = conn.raw(&commands::zrange(key, r, r + limit as i64 - 1)).await?;
            // RESP2 answers a flat member/score list, RESP3 an array of pairs
            let Value::Array(items) = v else {
                return Err(Error::reply("ZRANGE", "expected an array"));
            };
            let nested = items.first().is_some_and(|i| matches!(i, Value::Array(_)));
            let flat = if nested {
                let mut flat = Vec::with_capacity(items.len() * 2);
                for i in items {
                    flat.extend(bulk_list(i, "ZRANGE")?);
                }
                flat
            } else {
                bulk_list(Value::Array(items), "ZRANGE")?
            };
            let entries: Vec<Entry> = flat
                .chunks(2)
                .map(|p| Entry {
                    field: p[0].clone(),
                    value: p.get(1).cloned().unwrap_or_default(),
                    score: p.get(1).and_then(|s| parse_f64(s)),
                })
                .collect();
            let n = entries.len();
            Ok(ValuePage {
                entries,
                cursor: 0,
                next: Some((r + n as i64).to_string()),
                done: n < limit,
            })
        }
        ("zset", c) => {
            let mut p = scan_page(conn, commands::zscan(key, scan_cursor(c)?, filter, limit), true).await?;
            for e in &mut p.entries {
                e.score = parse_f64(&e.value);
            }
            Ok(p)
        }
        ("list", c) => {
            let start = match c {
                Cursor::Start => 0,
                Cursor::Rank(r) => r,
                other => return Err(Error::Config(format!("list pages take a rank cursor, not {other:?}"))),
            };
            let v = conn.raw(&commands::lrange(key, start, start + limit as i64 - 1)).await?;
            let items = bulk_list(v, "LRANGE")?;
            let n = items.len();
            let entries = items
                .into_iter()
                .map(|i| Entry {
                    field: i,
                    value: Vec::new(),
                    score: None,
                })
                .collect();
            Ok(ValuePage {
                entries,
                cursor: 0,
                next: Some((start + n as i64).to_string()),
                done: n < limit,
            })
        }
        ("stream", c) => {
            let start = match c {
                Cursor::Start => "-".to_string(),
                Cursor::StreamId(id) => format!("({id}"),
                other => return Err(Error::Config(format!("stream pages take an id cursor, not {other:?}"))),
            };
            let v = conn.raw(&commands::xrange(key, &start, limit)).await?;
            let Value::Array(items) = v else {
                return Err(Error::reply("XRANGE", "expected an array"));
            };
            let mut entries = Vec::with_capacity(items.len());
            for item in items {
                let Value::Array(mut parts) = item else { continue };
                if parts.len() != 2 {
                    continue;
                }
                let fields = parts.pop().unwrap();
                let id = value_text(&parts.pop().unwrap());
                let flat = bulk_list(fields, "XRANGE")?;
                let mut text = Vec::new();
                for p in flat.chunks(2) {
                    if !text.is_empty() {
                        text.extend_from_slice(b", ");
                    }
                    text.extend_from_slice(&p[0]);
                    text.push(b'=');
                    if let Some(v) = p.get(1) {
                        text.extend_from_slice(v);
                    }
                }
                entries.push(Entry {
                    field: id.into_bytes(),
                    value: text,
                    score: None,
                });
            }
            let n = entries.len();
            let next = entries.last().map(|e| String::from_utf8_lossy(&e.field).into_owned());
            Ok(ValuePage {
                entries,
                cursor: 0,
                next,
                done: n < limit,
            })
        }
        (other, _) => Err(Error::Config(format!("no pager for type {other:?}"))),
    }
}

fn scan_cursor(c: Cursor) -> Result<u64> {
    match c {
        Cursor::Start => Ok(0),
        Cursor::Scan(n) => Ok(n),
        other => Err(Error::Config(format!("scan pages take a scan cursor, not {other:?}"))),
    }
}

async fn scan_page(conn: &mut Conn, cmd: redis::Cmd, pairs: bool) -> Result<ValuePage> {
    let v = conn.raw(&cmd).await?;
    let (cursor, flat) = crate::scan::parse_scan_reply(v)?;
    let entries = if pairs {
        flat.chunks(2)
            .map(|p| Entry {
                field: p[0].clone(),
                value: p.get(1).cloned().unwrap_or_default(),
                score: None,
            })
            .collect()
    } else {
        flat.into_iter()
            .map(|f| Entry {
                field: f,
                value: Vec::new(),
                score: None,
            })
            .collect()
    };
    Ok(ValuePage {
        entries,
        cursor,
        next: None,
        done: cursor == 0,
    })
}

fn bulk_list(v: Value, command: &str) -> Result<Vec<Vec<u8>>> {
    let Value::Array(items) = v else {
        return Err(Error::reply(command, "expected an array"));
    };
    items
        .into_iter()
        .map(|i| match i {
            Value::BulkString(b) => Ok(b),
            Value::SimpleString(s) => Ok(s.into_bytes()),
            Value::Int(n) => Ok(n.to_string().into_bytes()),
            Value::Double(d) => Ok(d.to_string().into_bytes()),
            other => Err(Error::reply(command, format!("unexpected element {other:?}"))),
        })
        .collect()
}

fn parse_f64(b: &[u8]) -> Option<f64> {
    std::str::from_utf8(b).ok().and_then(|s| s.parse().ok())
}

/// A slice of a string value: `GETRANGE offset..offset+len-1`.
pub async fn string_chunk(conn: &mut Conn, key: &[u8], offset: i64, len: i64) -> Result<Vec<u8>> {
    let len = len.clamp(1, CHUNK_BYTES);
    match conn.raw(&commands::getrange(key, offset, offset + len - 1)).await? {
        Value::BulkString(b) => Ok(b),
        Value::SimpleString(s) => Ok(s.into_bytes()),
        Value::Nil => Ok(Vec::new()),
        other => Err(Error::reply("GETRANGE", format!("{other:?}"))),
    }
}

/// `DUMP` for a type without a viewer: the serialized length and a peek.
pub async fn dump_peek(conn: &mut Conn, key: &[u8], peek: usize) -> Result<(usize, Vec<u8>)> {
    match conn.raw(&commands::dump(key)).await? {
        Value::BulkString(b) => {
            let n = b.len();
            let mut p = b;
            p.truncate(peek);
            Ok((n, p))
        }
        Value::Nil => Ok((0, Vec::new())),
        other => Err(Error::reply("DUMP", format!("{other:?}"))),
    }
}

/// Content kind of a string value, decided on its preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Json,
    Text,
    Gzip,
    Msgpack,
    Binary,
    Empty,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Json => "json",
            Kind::Text => "text",
            Kind::Gzip => "gzip",
            Kind::Msgpack => "msgpack",
            Kind::Binary => "binary",
            Kind::Empty => "empty",
        }
    }
}

/// Pick a viewer from the first bytes. JSON is recognised when the preview
/// starts like a document and, if it is the whole value, parses; text
/// needs valid UTF-8 with few control characters; gzip and msgpack by
/// their leading bytes; everything else is binary.
pub fn detect(preview: &[u8], complete: bool) -> Kind {
    if preview.is_empty() {
        return Kind::Empty;
    }
    if preview.len() >= 2 && preview[0] == 0x1f && preview[1] == 0x8b {
        return Kind::Gzip;
    }
    let first = preview.iter().position(|b| !b.is_ascii_whitespace()).map(|i| preview[i]);
    if let Ok(text) = std::str::from_utf8(preview) {
        let control = text.chars().filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')).count();
        if control * 50 <= text.len() {
            if matches!(first, Some(b'{') | Some(b'[')) && (!complete || json_balanced(text)) {
                return Kind::Json;
            }
            return Kind::Text;
        }
    } else if let Some(valid) = utf8_prefix_ok(preview)
        && !complete
        && valid
    {
        // a text value cut mid-character by the preview boundary
        return if matches!(first, Some(b'{') | Some(b'[')) {
            Kind::Json
        } else {
            Kind::Text
        };
    }
    match preview[0] {
        0x80..=0x9f | 0xdc..=0xdf => Kind::Msgpack,
        _ => Kind::Binary,
    }
}

/// Valid UTF-8 up to at most 3 trailing bytes of a cut sequence.
fn utf8_prefix_ok(b: &[u8]) -> Option<bool> {
    match std::str::from_utf8(b) {
        Ok(_) => Some(true),
        Err(e) => Some(e.error_len().is_none() && e.valid_up_to() + 4 > b.len()),
    }
}

/// Cheap structural check: brackets balance outside of strings.
fn json_balanced(text: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    for c in text.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_str
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_kinds() {
        assert_eq!(detect(b"", true), Kind::Empty);
        assert_eq!(detect(b"{\"a\": [1, 2]}", true), Kind::Json);
        assert_eq!(detect(b"  [1,2,3]", true), Kind::Json);
        assert_eq!(detect(b"{\"a\": [1, 2", true), Kind::Text);
        assert_eq!(detect(b"{\"a\": [1, 2", false), Kind::Json);
        assert_eq!(detect(b"hello\nworld", true), Kind::Text);
        assert_eq!(detect(&[0x1f, 0x8b, 0x08, 0x00], true), Kind::Gzip);
        assert_eq!(detect(&[0x82, 0xa1, b'a', 0x01], true), Kind::Msgpack);
        assert_eq!(detect(&[0x00, 0x01, 0x02, 0xff], true), Kind::Binary);
        let mut cut = "héllo wö".as_bytes().to_vec();
        cut.truncate(cut.len() - 1); // cuts the two-byte ö in half
        assert_eq!(detect(&cut, false), Kind::Text);
        assert_eq!(detect(&cut, true), Kind::Binary);
    }

    #[test]
    fn json_balance() {
        assert!(json_balanced("{\"a\":\"}\"}"));
        assert!(!json_balanced("{\"a\":1"));
        assert!(!json_balanced("]"));
    }
}
