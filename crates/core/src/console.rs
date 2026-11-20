//! The command console (CONCEPT 8, "command console"): the user types a
//! command, the server's reply comes back as lines. The browser's own
//! commands are the ones in [`crate::commands`]; what runs here is the
//! user's, under the user's ACL.
//!
//! The pieces, each usable on its own (the CLI's `exec` drives them all):
//! [`split_line`] turns a typed line into arguments the way `redis-cli`
//! does; [`classify`] decides from the server's own description of a
//! command (`COMMAND INFO`) whether it runs, is refused or needs a
//! confirmation; [`Console::run`] routes and sends it on a connection of
//! its own; [`reconcile`] reads the touched keys again so the index can
//! follow; [`render`] turns the reply into a bounded number of lines.
//!
//! The console runs commands that answer once. A command that changes the
//! state of its connection, blocks, or streams is refused: the connection
//! is multiplexed, and a tool for transactions or `MONITOR` is a different
//! tool.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use redis::Value;

use crate::cluster::{NodeAddr, key_slot};
use crate::commands;
use crate::error::{Error, Result};
use crate::meta;
use crate::mutate::Outcome;
use crate::route::{Hop, Router};

// ---- the typed line ------------------------------------------------------------

/// Split a typed line into arguments like `redis-cli`: separated by
/// whitespace; `"..."` with the escapes `\n \r \t \b \a \\ \"` and `\xHH`;
/// `'...'` taken literally except `\'`. A quote that is not closed, or a
/// closing quote glued to the next argument, is an error.
pub fn split_line(line: &str) -> Result<Vec<Vec<u8>>> {
    let bytes = line.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    let bad = |what: &str| Error::Config(format!("cannot read the command: {what}"));
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return Ok(args);
        }
        let mut arg = Vec::new();
        match bytes[i] {
            b'"' => {
                i += 1;
                loop {
                    let Some(&b) = bytes.get(i) else {
                        return Err(bad("a double quote is not closed"));
                    };
                    i += 1;
                    match b {
                        b'"' => break,
                        b'\\' => {
                            let Some(&e) = bytes.get(i) else {
                                return Err(bad("a double quote is not closed"));
                            };
                            i += 1;
                            let hex = |d: u8| (d as char).to_digit(16).map(|v| v as u8);
                            match e {
                                b'x' => match (bytes.get(i).copied().and_then(hex), bytes.get(i + 1).copied().and_then(hex)) {
                                    (Some(hi), Some(lo)) => {
                                        arg.push(hi << 4 | lo);
                                        i += 2;
                                    }
                                    // like redis-cli: not an escape, the letter itself
                                    _ => arg.push(b'x'),
                                },
                                b'n' => arg.push(b'\n'),
                                b'r' => arg.push(b'\r'),
                                b't' => arg.push(b'\t'),
                                b'b' => arg.push(0x08),
                                b'a' => arg.push(0x07),
                                other => arg.push(other),
                            }
                        }
                        other => arg.push(other),
                    }
                }
                if bytes.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
                    return Err(bad("a closing quote must be followed by a space"));
                }
            }
            b'\'' => {
                i += 1;
                loop {
                    let Some(&b) = bytes.get(i) else {
                        return Err(bad("a single quote is not closed"));
                    };
                    i += 1;
                    match b {
                        b'\'' => break,
                        b'\\' if bytes.get(i) == Some(&b'\'') => {
                            arg.push(b'\'');
                            i += 1;
                        }
                        other => arg.push(other),
                    }
                }
                if bytes.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
                    return Err(bad("a closing quote must be followed by a space"));
                }
            }
            _ => {
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    arg.push(bytes[i]);
                    i += 1;
                }
            }
        }
        args.push(arg);
    }
}

// ---- what a command is ---------------------------------------------------------

/// What the server says about a command (`COMMAND INFO`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandInfo {
    /// Lowercase, a subcommand in pipe notation (`config|set`).
    pub name: String,
    pub arity: i64,
    /// `write`, `readonly`, `admin`, `blocking`, `may_replicate`, ...
    pub flags: Vec<String>,
    /// ACL categories with their `@`: `@dangerous`, `@read`, ...
    pub categories: Vec<String>,
}

impl CommandInfo {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    pub fn in_category(&self, category: &str) -> bool {
        self.categories.iter().any(|c| c == category)
    }
}

fn strings(v: &Value) -> Vec<String> {
    let items: &[Value] = match v {
        Value::Array(a) | Value::Set(a) => a,
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|x| match x {
            Value::SimpleString(s) => Some(s.to_ascii_lowercase()),
            Value::BulkString(b) => Some(String::from_utf8_lossy(b).to_ascii_lowercase()),
            Value::Okay => Some("ok".into()),
            _ => None,
        })
        .collect()
}

/// One entry of a `COMMAND INFO` reply; `None` for a command the server
/// does not know.
pub fn parse_command_info(entry: &Value) -> Option<CommandInfo> {
    let Value::Array(parts) = entry else { return None };
    let name = match parts.first()? {
        Value::BulkString(b) => String::from_utf8_lossy(b).to_ascii_lowercase(),
        Value::SimpleString(s) => s.to_ascii_lowercase(),
        _ => return None,
    };
    Some(CommandInfo {
        name,
        arity: match parts.get(1) {
            Some(Value::Int(n)) => *n,
            _ => 0,
        },
        flags: parts.get(2).map(strings).unwrap_or_default(),
        categories: parts.get(6).map(strings).unwrap_or_default(),
    })
}

/// Why the console asks before it runs a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// A write on a prod-tagged connection.
    Write,
    /// The command can hurt the server; the text says how.
    Danger(String),
    /// The connection's name must be typed; the text says what happens.
    Typed(String),
}

/// What to do with a typed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Run,
    /// Not run by the console; the text says why.
    Refuse(String),
    Confirm(Confirm),
}

/// What a command may have done to the keyspace, for the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Read-only, or it failed before reaching the server.
    None,
    /// These keys may have changed; reading them again settles it.
    Keys(Vec<Vec<u8>>),
    /// The database (or all of them) was emptied.
    Flushed,
    /// It may have changed keys it did not name.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub action: Action,
    /// The command may change data or the server (a write, a script, an
    /// administrative command), or the server did not say what it is.
    pub mutating: bool,
    /// It may change keys (as opposed to configuration).
    pub writes_keys: bool,
}

/// Uppercase command name and, for a container command, its subcommand.
fn names(args: &[Vec<u8>]) -> (String, Option<String>) {
    let up = |a: &Vec<u8>| String::from_utf8_lossy(a).to_ascii_uppercase();
    (args.first().map(up).unwrap_or_default(), args.get(1).map(up))
}

const STATEFUL: &[&str] = &[
    "SELECT",
    "MULTI",
    "EXEC",
    "DISCARD",
    "WATCH",
    "UNWATCH",
    "AUTH",
    "HELLO",
    "RESET",
    "QUIT",
    "READONLY",
    "READWRITE",
    "ASKING",
];
const CLIENT_STATE: &[&str] = &["REPLY", "TRACKING", "CACHING", "NO-EVICT", "NO-TOUCH"];
const STREAMING: &[&str] = &[
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "UNSUBSCRIBE",
    "PUNSUBSCRIBE",
    "SUNSUBSCRIBE",
    "MONITOR",
    "SYNC",
    "PSYNC",
    "REPLCONF",
];
/// Blocking commands by name, for a server that does not describe them.
const BLOCKING: &[&str] = &[
    "BLPOP",
    "BRPOP",
    "BRPOPLPUSH",
    "BLMOVE",
    "BLMPOP",
    "BZPOPMIN",
    "BZPOPMAX",
    "BZMPOP",
    "WAIT",
    "WAITAOF",
];
/// Commands that run a script, for a server that does not flag them.
const SCRIPT_RUNNERS: &[&str] = &["EVAL", "EVALSHA", "FCALL"];
/// Read `BLOCK` only when asked to.
const BLOCK_OPTION: &[&str] = &["XREAD", "XREADGROUP"];
const TYPED: &[(&str, &str)] = &[
    ("FLUSHALL", "FLUSHALL deletes every key of every database on the server."),
    ("FLUSHDB", "FLUSHDB deletes every key of this database."),
    ("SHUTDOWN", "SHUTDOWN stops the server."),
    ("SWAPDB", "SWAPDB exchanges two databases for every client of the server."),
    ("DEBUG", "DEBUG can crash, block or corrupt the server."),
];
/// Administrative commands that only read: they are in the server's
/// `@dangerous` category for what they reveal, not for what they do.
const HARMLESS_ADMIN: &[&str] = &[
    "CONFIG|GET",
    "SLOWLOG|GET",
    "SLOWLOG|LEN",
    "CLIENT|LIST",
    "CLIENT|INFO",
    "CLIENT|ID",
    "CLIENT|GETNAME",
    "LATENCY|LATEST",
    "LATENCY|HISTORY",
    "LATENCY|DOCTOR",
    "MEMORY|DOCTOR",
    "MEMORY|STATS",
    "ACL|LIST",
    "ACL|GETUSER",
    "ACL|USERS",
    "ACL|CAT",
    "ACL|WHOAMI",
    "CLUSTER|INFO",
    "CLUSTER|NODES",
    "CLUSTER|SHARDS",
    "CLUSTER|SLOTS",
    "CLUSTER|MYID",
    "CLUSTER|KEYSLOT",
    "CLUSTER|COUNTKEYSINSLOT",
    "MODULE|LIST",
    "ROLE",
    "LASTSAVE",
    "INFO",
    "DBSIZE",
    "COMMAND",
];

/// Decide what the console does with a command. `info` is the server's
/// description of it, `None` when the server did not give one (an unknown
/// command, or `COMMAND` denied): the stateful and streaming commands are
/// then still refused by name, and everything else counts as a write.
pub fn classify(args: &[Vec<u8>], info: Option<&CommandInfo>, prod: bool) -> Verdict {
    let (name, sub) = names(args);
    let full = match &sub {
        Some(s) => format!("{name}|{s}"),
        None => name.clone(),
    };
    let refuse = |why: String| Verdict {
        action: Action::Refuse(why),
        mutating: false,
        writes_keys: false,
    };
    if name.is_empty() {
        return refuse("there is no command".into());
    }
    if STATEFUL.contains(&name.as_str()) || (name == "CLIENT" && sub.as_deref().is_some_and(|s| CLIENT_STATE.contains(&s))) {
        return refuse(format!(
            "{} changes the state of its connection; the console runs commands that answer once and leave it as it was",
            if name == "CLIENT" { full.replace('|', " ") } else { name.clone() }
        ));
    }
    if STREAMING.contains(&name.as_str()) {
        return refuse(format!(
            "{name} turns the connection into a stream; the console runs commands that answer once"
        ));
    }
    let asks_to_block = args.iter().skip(1).any(|a| a.eq_ignore_ascii_case(b"BLOCK"));
    let blocking = if BLOCK_OPTION.contains(&name.as_str()) {
        asks_to_block
    } else {
        BLOCKING.contains(&name.as_str()) || info.is_some_and(|i| i.has("blocking"))
    };
    if blocking {
        return refuse(format!(
            "{name} can block until something happens; the console runs commands that answer at once"
        ));
    }

    let harmless = HARMLESS_ADMIN.contains(&full.as_str()) || HARMLESS_ADMIN.contains(&name.as_str());
    // a script can write whatever its command's flags say; the server marks
    // the read-only variants (EVAL_RO, FCALL_RO) as such
    let script = SCRIPT_RUNNERS.contains(&name.as_str()) || info.is_some_and(|i| i.has("script_runner"));
    let (writes_keys, admin, dangerous) = match info {
        Some(i) => (
            i.has("write") || i.has("may_replicate") || script && !i.has("readonly"),
            i.has("admin") && !harmless,
            i.in_category("@dangerous") && !harmless,
        ),
        // not described: assume the worst that still lets it run
        None => (!harmless, false, false),
    };
    let mutating = writes_keys || admin;
    let action = if let Some((_, what)) = TYPED.iter().find(|(n, _)| *n == name) {
        Action::Confirm(Confirm::Typed((*what).to_string()))
    } else if name == "KEYS" {
        Action::Confirm(Confirm::Danger(
            "KEYS walks every key in one call and blocks the server for as long as that takes. SCAN does the same in steps.".into(),
        ))
    } else if dangerous && mutating {
        Action::Confirm(Confirm::Danger(format!(
            "The server lists {} as dangerous: it changes the server or its data beyond one key.",
            full.replace('|', " ")
        )))
    } else if prod && mutating {
        Action::Confirm(Confirm::Write)
    } else {
        Action::Run
    };
    Verdict {
        action,
        mutating,
        writes_keys,
    }
}

// ---- running it ----------------------------------------------------------------

/// What came of a command.
#[derive(Debug)]
pub struct Ran {
    /// The reply, or the error the server (or the connection) answered with.
    pub reply: Result<Value>,
    /// The node that answered.
    pub node: NodeAddr,
    pub effect: Effect,
    pub took: Duration,
}

/// A console on a router of its own. It caches what the server said about
/// the commands it has seen.
pub struct Console {
    router: Router,
    info: HashMap<String, Option<CommandInfo>>,
    /// `COMMAND INFO` is denied or unknown on this server.
    undescribed: bool,
}

impl Console {
    /// `router` must be the console's own, never one the browser uses.
    pub fn new(router: Router) -> Console {
        Console {
            router,
            info: HashMap::new(),
            undescribed: false,
        }
    }

    pub fn router(&mut self) -> &mut Router {
        &mut self.router
    }

    /// The server's description of the command in `args`, from the cache
    /// or one `COMMAND INFO` call; the subcommand's entry when there is one.
    pub async fn describe(&mut self, args: &[Vec<u8>]) -> Result<Option<CommandInfo>> {
        let (name, sub) = names(args);
        if name.is_empty() || self.undescribed {
            return Ok(None);
        }
        let name = name.to_ascii_lowercase();
        let full = sub.map(|s| format!("{name}|{}", s.to_ascii_lowercase()));
        let wanted: Vec<&str> = full.iter().map(String::as_str).chain([name.as_str()]).collect();
        if wanted.iter().any(|w| !self.info.contains_key(*w)) {
            let seed = self.router.seed().clone();
            let cmd = commands::command_info(&wanted);
            match crate::routed!(self.router, Hop::node(&seed), |c| c.raw(&cmd).await) {
                Ok(Value::Array(entries)) => {
                    for (w, e) in wanted.iter().zip(entries.iter()) {
                        self.info.insert((*w).to_string(), parse_command_info(e));
                    }
                }
                Ok(_) => {}
                // denied, or a server without COMMAND INFO: classify by name alone
                Err(Error::Denied { .. }) | Err(Error::Server { .. }) => self.undescribed = true,
                Err(e) => return Err(e),
            }
        }
        Ok(wanted.iter().find_map(|w| self.info.get(*w).cloned().flatten()))
    }

    /// [`classify`] with the server's description.
    pub async fn inspect(&mut self, args: &[Vec<u8>], prod: bool) -> Result<Verdict> {
        let info = self.describe(args).await?;
        Ok(classify(args, info.as_ref(), prod))
    }

    /// The keys a command names, as the server sees them. A command
    /// without keys, or a server that will not say, gives none.
    async fn keys_of(&mut self, args: &[Vec<u8>]) -> Result<Option<Vec<Vec<u8>>>> {
        let seed = self.router.seed().clone();
        let cmd = commands::command_getkeys(args);
        match crate::routed!(self.router, Hop::node(&seed), |c| c.raw(&cmd).await) {
            Ok(Value::Array(keys)) => Ok(Some(
                keys.into_iter()
                    .filter_map(|k| match k {
                        Value::BulkString(b) => Some(b),
                        Value::SimpleString(s) => Some(s.into_bytes()),
                        _ => None,
                    })
                    .collect(),
            )),
            Ok(_) => Ok(Some(Vec::new())),
            // "The command has no key arguments", wrong arity, an unknown command
            Err(Error::Server { .. }) => Ok(Some(Vec::new())),
            Err(Error::Denied { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Send a command that [`classify`] let through (and the user
    /// confirmed, where that was asked). With keys it goes to the node
    /// that owns them, following redirects; without, to `node`, or the
    /// seed. On a cluster the keys must share a slot, which is checked
    /// here rather than left to a `CROSSSLOT` reply.
    pub async fn run(&mut self, args: &[Vec<u8>], node: Option<&NodeAddr>, verdict: &Verdict) -> Result<Ran> {
        if matches!(verdict.action, Action::Refuse(_)) {
            return Err(Error::Config("the console does not run this command".into()));
        }
        let (name, _) = names(args);
        let cluster = self.router.is_cluster();
        let keys = if cluster || verdict.writes_keys {
            self.keys_of(args).await?
        } else {
            Some(Vec::new())
        };
        let hop = match (&keys, cluster) {
            (Some(keys), true) if !keys.is_empty() => {
                let slot = key_slot(&keys[0]);
                if let Some(other) = keys.iter().find(|k| key_slot(k) != slot) {
                    return Err(Error::Config(format!(
                        "the keys of this command are in different hash slots ({} in {slot}, {} in {}); on a cluster one command works on one slot",
                        String::from_utf8_lossy(&keys[0]),
                        String::from_utf8_lossy(other),
                        key_slot(other)
                    )));
                }
                Hop::key(&keys[0])
            }
            _ => Hop::node(node.unwrap_or(&self.router.seed().clone())),
        };
        let cmd = commands::console::user(args);
        let started = Instant::now();
        let mut answered = self.router.seed().clone();
        let reply = crate::routed!(self.router, hop, |c| {
            answered = c.addr().clone();
            c.raw(&cmd).await
        });
        let took = started.elapsed();
        let transport = matches!(&reply, Err(Error::Client(_)) | Err(Error::Io(_)));
        let effect = if !verdict.writes_keys || transport && keys.as_ref().is_none_or(Vec::is_empty) {
            Effect::None
        } else if name == "FLUSHDB" || name == "FLUSHALL" {
            if reply.is_ok() { Effect::Flushed } else { Effect::None }
        } else {
            match keys {
                Some(keys) if !keys.is_empty() => Effect::Keys(keys),
                // a script without declared keys, SWAPDB, a write the server would not explain
                _ if reply.is_ok() => Effect::Unknown,
                _ => Effect::None,
            }
        };
        Ok(Ran {
            reply,
            node: answered,
            effect,
            took,
        })
    }
}

/// Read keys again after a command may have changed them: one outcome per
/// key, saying whether it exists now and with what expiry, in the shape
/// the index's write-through takes.
pub async fn reconcile(router: &mut Router, keys: &[Vec<u8>]) -> Result<Vec<Outcome>> {
    let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
    let metas = meta::fetch_routed(router, &refs).await?;
    Ok(keys
        .iter()
        .zip(metas)
        .map(|(k, m)| Outcome {
            removed: if m.exists { Vec::new() } else { vec![k.clone()] },
            added: if m.exists { vec![k.clone()] } else { Vec::new() },
            ttl: m.ttl,
            reply: String::new(),
        })
        .collect())
}

// ---- the reply as lines --------------------------------------------------------

/// How much of a reply is turned into lines.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Elements shown of one array, map or set.
    pub elements: usize,
    /// Lines in all.
    pub lines: usize,
    /// Bytes shown of one string.
    pub string_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            elements: 200,
            lines: 2000,
            string_bytes: 4096,
        }
    }
}

/// A reply as text, in the style of `redis-cli`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub lines: Vec<String>,
    /// Something was left out; the lines say where.
    pub truncated: bool,
}

/// A byte string between double quotes: printable text as it is, the
/// quote and the backslash escaped, everything else as `\n`, `\r`, `\t`
/// or `\xNN`. What [`split_line`] reads back.
pub fn quoted(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out.push('"');
    out
}

struct Renderer<'a> {
    limits: &'a Limits,
    budget: usize,
    truncated: bool,
}

impl Renderer<'_> {
    fn string(&mut self, bytes: &[u8]) -> Vec<String> {
        let cut = bytes.len() > self.limits.string_bytes;
        let shown = if cut { &bytes[..self.limits.string_bytes] } else { bytes };
        let note = if cut {
            self.truncated = true;
            format!(" … ({} bytes in all)", bytes.len())
        } else {
            String::new()
        };
        // text of several lines (INFO, CLUSTER NODES, a script) reads better as lines
        match std::str::from_utf8(shown) {
            Ok(text) if text.trim_end().contains('\n') && !text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) => {
                let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
                if cut {
                    lines.push(note.trim_start().to_string());
                }
                lines
            }
            _ => vec![format!("{}{note}", quoted(shown))],
        }
    }

    /// The lines of an aggregate: every element's lines behind its marker
    /// (`1) `, `1# `, `1~ `), continuation lines indented to match.
    fn items(&mut self, items: Vec<&Value>, empty: &str, marker: char, lead: &str) -> Vec<String> {
        if items.is_empty() {
            return vec![empty.to_string()];
        }
        let total = items.len();
        let shown = total.min(self.limits.elements);
        let width = shown.to_string().len();
        let mut out = Vec::new();
        for (i, item) in items.into_iter().take(shown).enumerate() {
            if self.budget == 0 {
                self.truncated = true;
                out.push(format!("… {} more", total - i));
                return out;
            }
            let prefix = format!("{lead}{:>width$}{marker} ", i + 1);
            let pad = " ".repeat(prefix.chars().count());
            for (j, line) in self.value(item).into_iter().enumerate() {
                out.push(format!("{}{line}", if j == 0 { &prefix } else { &pad }));
            }
        }
        if shown < total {
            self.truncated = true;
            out.push(format!("… {} more", total - shown));
        }
        out
    }

    fn value(&mut self, v: &Value) -> Vec<String> {
        if self.budget == 0 {
            self.truncated = true;
            return vec!["…".into()];
        }
        let lines = match v {
            Value::Nil => vec!["(nil)".into()],
            Value::Int(n) => vec![format!("(integer) {n}")],
            Value::BulkString(b) => self.string(b),
            Value::SimpleString(s) => vec![s.clone()],
            Value::Okay => vec!["OK".into()],
            Value::Double(d) => vec![format!("(double) {d}")],
            Value::Boolean(b) => vec![format!("({b})")],
            Value::VerbatimString { text, .. } => self.string(text.as_bytes()),
            Value::BigNumber(n) => vec![format!("(big number) {n:?}")],
            Value::ServerError(e) => vec![
                format!("(error) {} {}", e.code(), e.details().unwrap_or_default())
                    .trim_end()
                    .to_string(),
            ],
            Value::Array(a) => self.items(a.iter().collect(), "(empty array)", ')', ""),
            Value::Set(a) => self.items(a.iter().collect(), "(empty set)", '~', ""),
            Value::Push { data, .. } => self.items(data.iter().collect(), "(empty push)", ')', ">"),
            Value::Attribute { data, .. } => self.value(data),
            Value::Map(pairs) => {
                if pairs.is_empty() {
                    vec!["(empty hash)".into()]
                } else {
                    let total = pairs.len();
                    let shown = total.min(self.limits.elements);
                    let width = shown.to_string().len();
                    let mut out = Vec::new();
                    for (i, (k, val)) in pairs.iter().take(shown).enumerate() {
                        if self.budget == 0 {
                            break;
                        }
                        let key = self.value(k).join(" ");
                        let prefix = format!("{:>width$}# {key} => ", i + 1);
                        let pad = " ".repeat(prefix.chars().count());
                        for (j, line) in self.value(val).into_iter().enumerate() {
                            out.push(format!("{}{line}", if j == 0 { &prefix } else { &pad }));
                        }
                    }
                    if shown < total {
                        self.truncated = true;
                        out.push(format!("… {} more", total - shown));
                    }
                    out
                }
            }
            // the enum is open: a kind of reply this build does not know
            other => vec![format!("{other:?}")],
        };
        self.budget = self.budget.saturating_sub(match v {
            Value::Array(_) | Value::Set(_) | Value::Map(_) | Value::Push { .. } | Value::Attribute { .. } => 0,
            _ => lines.len(),
        });
        lines
    }
}

/// A reply as lines, cut to `limits` with every cut marked.
pub fn render(v: &Value, limits: &Limits) -> Rendered {
    let mut r = Renderer {
        limits,
        budget: limits.lines.max(1),
        truncated: false,
    };
    let mut lines = r.value(v);
    if lines.len() > limits.lines {
        let more = lines.len() - limits.lines;
        lines.truncate(limits.lines);
        lines.push(format!("… {more} more lines"));
        r.truncated = true;
    }
    Rendered {
        lines,
        truncated: r.truncated,
    }
}

/// An error as the console shows it.
pub fn render_error(e: &Error) -> String {
    match e {
        Error::Server { code, detail } => format!("(error) {code} {detail}").trim_end().to_string(),
        Error::Denied { command } => format!("(error) NOPERM the server's ACL does not allow {command} for this user"),
        other => format!("(error) {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn lines_split_like_redis_cli() {
        let ok = |line: &str| split_line(line).unwrap();
        assert_eq!(ok("GET user:1"), args(&["GET", "user:1"]));
        assert_eq!(ok("  set   k   v  "), args(&["set", "k", "v"]));
        assert!(ok("").is_empty() && ok("   \t ").is_empty());
        assert_eq!(ok(r#"SET k "two words""#), args(&["SET", "k", "two words"]));
        assert_eq!(ok(r#"SET k """#), args(&["SET", "k", ""]));
        assert_eq!(ok(r#"SET "a\"b" 'it\'s' 'x\ny'"#), args(&["SET", "a\"b", "it's", "x\\ny"]));
        assert_eq!(
            ok(r#"SET "bin:\xff\x00\x41" "\n\r\t\b\a\\\q""#),
            vec![b"SET".to_vec(), b"bin:\xff\x00A".to_vec(), b"\n\r\t\x08\x07\\q".to_vec()]
        );
        // not hex: the letter itself, as redis-cli does
        assert_eq!(ok(r#""\xzz""#), vec![b"xzz".to_vec()]);
        // text outside ASCII passes through as its bytes
        assert_eq!(ok("GET käse"), vec![b"GET".to_vec(), "käse".as_bytes().to_vec()]);
        for bad in [r#"GET "open"#, "GET 'open", r#"GET "a"b"#, "GET 'a'b", r#"GET "a\"#] {
            assert!(split_line(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn quoting_round_trips_through_the_splitter() {
        let all: Vec<u8> = (0..=255u8).collect();
        for bytes in [&all[..], b"", b"plain", b"a \"b\" \\ c", "käse\n".as_bytes(), b"\xff\xfe"] {
            assert_eq!(split_line(&quoted(bytes)).unwrap(), vec![bytes.to_vec()], "{}", quoted(bytes));
        }
        assert_eq!(quoted(b"a\"b\\\n\x00\xff"), r#""a\"b\\\n\x00\xff""#);
    }

    fn info(name: &str, flags: &[&str], categories: &[&str]) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            arity: 0,
            flags: flags.iter().map(|s| s.to_string()).collect(),
            categories: categories.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn commands_are_classified_from_what_the_server_says() {
        let get = info("get", &["readonly", "fast"], &["@read", "@string", "@fast"]);
        let set = info("set", &["write", "denyoom"], &["@write", "@string", "@slow"]);
        let run = |a: &[&str], i: Option<&CommandInfo>, prod: bool| classify(&args(a), i, prod);

        let v = run(&["GET", "k"], Some(&get), true);
        assert_eq!((v.action, v.mutating), (Action::Run, false));
        let v = run(&["set", "k", "v"], Some(&set), false);
        assert_eq!((v.action.clone(), v.mutating, v.writes_keys), (Action::Run, true, true));
        // prod confirms what is not known to be read-only
        assert_eq!(run(&["SET", "k", "v"], Some(&set), true).action, Action::Confirm(Confirm::Write));
        assert_eq!(run(&["FOO.BAR", "k"], None, true).action, Action::Confirm(Confirm::Write));
        assert!(run(&["FOO.BAR", "k"], None, false).writes_keys);
        // a script may write without the write flag
        let eval = info("eval", &["noscript", "stale", "may_replicate"], &["@slow", "@scripting"]);
        assert!(run(&["EVAL", "return 1", "0"], Some(&eval), false).writes_keys);
        // newer servers flag the runner instead, and mark the read-only variants
        let eval = info("eval", &["noscript", "stale", "script_runner"], &["@slow", "@scripting"]);
        assert!(run(&["EVAL", "return 1", "0"], Some(&eval), false).writes_keys);
        let fcall = info("fcall", &["noscript", "stale"], &["@slow", "@scripting"]);
        assert!(run(&["FCALL", "f", "0"], Some(&fcall), false).writes_keys);
        let eval_ro = info(
            "eval_ro",
            &["readonly", "noscript", "stale", "script_runner"],
            &["@slow", "@scripting"],
        );
        assert!(!run(&["EVAL_RO", "return 1", "0"], Some(&eval_ro), true).mutating);

        // refused whatever the server says, with the reason
        for line in [
            vec!["SELECT", "1"],
            vec!["multi"],
            vec!["EXEC"],
            vec!["AUTH", "x"],
            vec!["HELLO", "3"],
            vec!["CLIENT", "REPLY", "OFF"],
            vec!["client", "tracking", "on"],
            vec!["SUBSCRIBE", "c"],
            vec!["MONITOR"],
            vec!["BLPOP", "q", "0"],
            vec!["XREAD", "BLOCK", "0", "STREAMS", "s", "$"],
            vec!["WAIT", "1", "0"],
        ] {
            let v = run(&line, None, false);
            assert!(matches!(v.action, Action::Refuse(_)), "{line:?}: {v:?}");
        }
        let xread = info("xread", &["readonly", "blocking"], &["@read", "@stream", "@blocking"]);
        assert_eq!(
            run(&["XREAD", "COUNT", "5", "STREAMS", "s", "0"], Some(&xread), true).action,
            Action::Run
        );
        let brpop = info("brpop", &["write", "blocking"], &["@write", "@list", "@blocking"]);
        assert!(matches!(run(&["BRPOP", "q", "1"], Some(&brpop), false).action, Action::Refuse(_)));
        assert_eq!(run(&["CLIENT", "LIST"], None, true).action, Action::Run);
        assert!(matches!(run(&[], None, false).action, Action::Refuse(_)));

        // typed confirmation on every tag
        let flushdb = info("flushdb", &["write"], &["@keyspace", "@write", "@slow", "@dangerous"]);
        for (line, i) in [
            (vec!["FLUSHDB"], Some(&flushdb)),
            (vec!["flushall", "ASYNC"], None),
            (vec!["DEBUG", "SLEEP", "1"], None),
        ] {
            assert!(
                matches!(run(&line, i, false).action, Action::Confirm(Confirm::Typed(_))),
                "{line:?}"
            );
        }
        // KEYS, and what the server calls dangerous and can change something
        let keys = info("keys", &["readonly"], &["@keyspace", "@read", "@slow", "@dangerous"]);
        assert!(matches!(
            run(&["KEYS", "*"], Some(&keys), false).action,
            Action::Confirm(Confirm::Danger(_))
        ));
        let config_set = info(
            "config|set",
            &["admin", "noscript", "loading", "stale"],
            &["@admin", "@slow", "@dangerous"],
        );
        let v = run(&["CONFIG", "SET", "maxmemory", "1"], Some(&config_set), false);
        assert!(
            matches!(v.action, Action::Confirm(Confirm::Danger(_))) && v.mutating && !v.writes_keys,
            "{v:?}"
        );
        // dangerous for what it reveals only: runs, also on prod
        let config_get = info(
            "config|get",
            &["admin", "noscript", "loading", "stale"],
            &["@admin", "@slow", "@dangerous"],
        );
        assert_eq!(run(&["CONFIG", "GET", "maxmemory"], Some(&config_get), true).action, Action::Run);
        let info_cmd = info("info", &["loading", "stale"], &["@slow", "@dangerous"]);
        assert_eq!(run(&["INFO"], Some(&info_cmd), true).action, Action::Run);
    }

    #[test]
    fn command_info_entries_are_read_in_both_protocols() {
        let s = |t: &str| Value::SimpleString(t.into());
        let b = |t: &str| Value::BulkString(t.as_bytes().to_vec());
        let resp2 = Value::Array(vec![
            b("SET"),
            Value::Int(-3),
            Value::Array(vec![s("write"), s("denyoom")]),
            Value::Int(1),
            Value::Int(1),
            Value::Int(1),
            Value::Array(vec![s("@write"), s("@string")]),
        ]);
        let i = parse_command_info(&resp2).unwrap();
        assert_eq!((i.name.as_str(), i.arity), ("set", -3));
        assert!(i.has("write") && i.in_category("@write") && !i.has("readonly"));
        let resp3 = Value::Array(vec![
            b("get"),
            Value::Int(2),
            Value::Set(vec![s("readonly"), s("fast")]),
            Value::Int(1),
            Value::Int(1),
            Value::Int(1),
            Value::Set(vec![s("@read")]),
        ]);
        assert!(parse_command_info(&resp3).unwrap().has("readonly"));
        assert_eq!(parse_command_info(&Value::Nil), None);
        // an old server without categories
        assert!(
            parse_command_info(&Value::Array(vec![b("x"), Value::Int(1)]))
                .unwrap()
                .categories
                .is_empty()
        );
    }

    #[test]
    fn replies_render_like_redis_cli() {
        let b = |t: &str| Value::BulkString(t.as_bytes().to_vec());
        let text = |v: &Value| render(v, &Limits::default()).lines.join("\n");
        assert_eq!(text(&Value::Nil), "(nil)");
        assert_eq!(text(&Value::Int(42)), "(integer) 42");
        assert_eq!(text(&Value::Okay), "OK");
        assert_eq!(text(&Value::SimpleString("PONG".into())), "PONG");
        assert_eq!(text(&b("a \"b\"")), r#""a \"b\"""#);
        assert_eq!(text(&Value::BulkString(b"\xff\x00".to_vec())), r#""\xff\x00""#);
        assert_eq!(text(&Value::Double(1.5)), "(double) 1.5");
        assert_eq!(text(&Value::Boolean(true)), "(true)");
        assert_eq!(text(&Value::Array(vec![])), "(empty array)");
        assert_eq!(text(&b("# Server\r\nredis_version:8\r\n")), "# Server\nredis_version:8");
        let nested = Value::Array(vec![
            b("a"),
            Value::Array(vec![Value::Int(1), Value::Nil]),
            Value::Map(vec![(b("k"), b("v")), (b("n"), Value::Array(vec![b("x"), b("y")]))]),
            Value::Set(vec![b("m")]),
        ]);
        assert_eq!(
            text(&nested),
            "1) \"a\"\n2) 1) (integer) 1\n   2) (nil)\n3) 1# \"k\" => \"v\"\n   2# \"n\" => 1) \"x\"\n             2) \"y\"\n4) 1~ \"m\""
        );
        // wide numbering stays aligned
        let twelve = Value::Array((0..12).map(Value::Int).collect());
        let lines = render(&twelve, &Limits::default()).lines;
        assert_eq!((lines[0].as_str(), lines[11].as_str()), (" 1) (integer) 0", "12) (integer) 11"));
    }

    #[test]
    fn replies_are_cut_and_say_so() {
        let limits = Limits {
            elements: 3,
            lines: 5,
            string_bytes: 4,
        };
        let r = render(&Value::Array((0..10).map(Value::Int).collect()), &limits);
        assert!(r.truncated);
        assert_eq!(r.lines, ["1) (integer) 0", "2) (integer) 1", "3) (integer) 2", "… 7 more"]);
        let r = render(&Value::BulkString(b"abcdefgh".to_vec()), &limits);
        assert_eq!(r.lines, ["\"abcd\" … (8 bytes in all)"]);
        assert!(r.truncated);
        // many short levels: the line budget ends it
        let deep = Value::Array((0..3).map(|_| Value::Array((0..3).map(Value::Int).collect())).collect());
        let r = render(&deep, &limits);
        assert!(r.truncated && r.lines.len() <= 6, "{:?}", r.lines);
        let whole = render(&Value::Array((0..3).map(Value::Int).collect()), &limits);
        assert!(!whole.truncated);
        // a million elements cost a page, not a million lines
        let big = Value::Array(vec![Value::Int(1); 1_000_000]);
        let r = render(&big, &Limits::default());
        assert_eq!(r.lines.len(), 201);
        assert_eq!(r.lines[200], "… 999800 more");
    }
}
