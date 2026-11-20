//! Search over the index (CONCEPT section 5.3): one input, three
//! interpretations chosen by syntax. Glob queries are a B-tree range scan
//! plus a glob filter; substring queries are a `LIKE` scan, or a trigram
//! lookup when the connection has opted into the FTS table (section 3.3);
//! regular expressions are streamed through the index in bounded windows.
//! Everything is cancellable: an interrupt handle on the search reader
//! aborts the running SQL statement and the Rust-side loops poll a flag.
//!
//! `verify` reconciles a glob against the server with a bounded
//! `SCAN MATCH`, for results drawn from a stale or partial index.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use redis::Value;

use crate::commands;
use crate::connection::Conn;
use crate::error::{Error, Result};
use crate::glob;
use crate::index::Index;
use crate::route::{Hop, Router};
use crate::scan::parse_scan_reply;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Glob,
    Substring,
    Regex,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Glob => "glob",
            Mode::Substring => "substring",
            Mode::Regex => "regex",
        }
    }
}

/// A parsed search input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub mode: Mode,
    /// The pattern body: the glob, the substring, or the regex source.
    pub term: Vec<u8>,
}

impl Query {
    /// Every key that starts with exactly these bytes: the glob `prefix*`
    /// with the prefix escaped, so glob characters in a key name match
    /// only themselves. Byte input, unlike `parse`, and nothing is trimmed.
    pub fn prefix(prefix: &[u8]) -> Query {
        let mut term = glob::escape(prefix);
        term.push(b'*');
        Query { mode: Mode::Glob, term }
    }
}

/// Classify an input by its syntax: `/.../` is a regex, anything with an
/// unescaped `*`, `?` or `[` is a glob, the rest is a substring. A plain
/// term is also an exact key name, which the UI opens directly on Enter
/// (section 5.4); that path needs no index and is not a search.
pub fn parse(input: &str) -> Query {
    let input = input.trim();
    if input.len() >= 2 && input.starts_with('/') && input.ends_with('/') {
        return Query {
            mode: Mode::Regex,
            term: input.as_bytes()[1..input.len() - 1].to_vec(),
        };
    }
    if glob::has_meta(input.as_bytes()) {
        return Query {
            mode: Mode::Glob,
            term: input.as_bytes().to_vec(),
        };
    }
    Query {
        mode: Mode::Substring,
        term: input.as_bytes().to_vec(),
    }
}

/// The Rust-side test applied to candidate names.
#[derive(Debug, Clone)]
pub enum Matcher {
    Glob(Vec<u8>),
    /// ASCII-lowercased needle; the index's `LIKE` folds ASCII case only,
    /// and this agrees with it.
    Substring(Vec<u8>),
    Regex(regex::bytes::Regex),
}

impl Matcher {
    pub fn matches(&self, name: &[u8]) -> bool {
        match self {
            Matcher::Glob(p) => glob::matches(p, name),
            Matcher::Substring(needle) => {
                if needle.is_empty() {
                    return true;
                }
                if needle.len() > name.len() {
                    return false;
                }
                name.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
            }
            Matcher::Regex(re) => re.is_match(name),
        }
    }
}

/// How a query runs against the index.
#[derive(Debug, Clone)]
pub struct Plan {
    pub query: Query,
    /// Byte range of candidate names: `[lo, hi)`; `hi == None` is open.
    pub lo: Vec<u8>,
    pub hi: Option<Vec<u8>>,
    /// A `LIKE` pattern (ESCAPE `\`) that every match satisfies, when the
    /// query has one; it narrows the SQL scan before the matcher runs.
    pub like: Option<Vec<u8>>,
    /// `true` when `like` alone decides membership (substring queries).
    pub like_is_exact: bool,
    /// The longest literal every match contains, when it is at least three
    /// characters, the query is not prefix-shaped and `like` needs no
    /// escaping (an `ESCAPE` clause disables the trigram index): the
    /// trigram table can then serve `like`, and this literal's rarest
    /// trigram decides whether it should (see `Index::fts_usable`).
    pub fts_literal: Option<String>,
    pub matcher: Matcher,
}

/// Largest regex the search accepts, in compiled size.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

impl Plan {
    pub fn new(query: Query) -> Result<Plan> {
        match query.mode {
            Mode::Glob => {
                let prefix = glob::literal_prefix(&query.term);
                let hi = glob::prefix_end(&prefix);
                let like = glob_to_like(&query.term);
                let literal = glob::longest_literal(&query.term);
                let fts_literal = fts_literal(prefix.is_empty(), &literal, &like);
                Ok(Plan {
                    lo: prefix,
                    hi,
                    like: Some(like),
                    like_is_exact: false,
                    fts_literal,
                    matcher: Matcher::Glob(query.term.clone()),
                    query,
                })
            }
            Mode::Substring => {
                let mut like = Vec::with_capacity(query.term.len() + 2);
                like.push(b'%');
                like.extend(like_escape(&query.term));
                like.push(b'%');
                let fts_literal = fts_literal(true, &query.term, &like);
                Ok(Plan {
                    lo: Vec::new(),
                    hi: None,
                    like: Some(like),
                    like_is_exact: true,
                    fts_literal,
                    matcher: Matcher::Substring(query.term.to_ascii_lowercase()),
                    query,
                })
            }
            Mode::Regex => {
                let source = std::str::from_utf8(&query.term).map_err(|_| Error::Config("regex is not UTF-8".into()))?;
                let re = regex::bytes::RegexBuilder::new(source)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|e| Error::Config(format!("regex: {e}")))?;
                let prefix = regex_prefix(source);
                let hi = glob::prefix_end(&prefix);
                Ok(Plan {
                    lo: prefix,
                    hi,
                    like: None,
                    like_is_exact: false,
                    fts_literal: None,
                    matcher: Matcher::Regex(re),
                    query,
                })
            }
        }
    }

    /// The glob to send as `SCAN MATCH` for a server-side verification;
    /// `None` for regular expressions, which the server cannot evaluate.
    pub fn server_pattern(&self) -> Option<Vec<u8>> {
        match self.query.mode {
            Mode::Glob => Some(self.query.term.clone()),
            Mode::Substring => {
                let mut p = vec![b'*'];
                p.extend(glob::escape(&self.query.term));
                p.push(b'*');
                Some(p)
            }
            Mode::Regex => None,
        }
    }
}

fn fts_literal(unprefixed: bool, literal: &[u8], like: &[u8]) -> Option<String> {
    if !unprefixed || like.contains(&b'\\') {
        return None;
    }
    let s = std::str::from_utf8(literal).ok()?;
    (s.chars().count() >= 3).then(|| s.to_string())
}

fn like_escape(literal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(literal.len());
    for &b in literal {
        if matches!(b, b'%' | b'_' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// A `LIKE` pattern matched by every name the glob matches (a superset:
/// `LIKE` folds ASCII case and a class becomes `_`).
fn glob_to_like(pattern: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pattern.len() + 2);
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'*' => {
                out.push(b'%');
                i += 1;
            }
            b'?' => {
                out.push(b'_');
                i += 1;
            }
            b'[' => {
                out.push(b'_');
                // skip to the closing bracket (or the end of the pattern)
                let mut j = i + 1;
                if pattern.get(j) == Some(&b'^') {
                    j += 1;
                }
                while j < pattern.len() {
                    if pattern[j] == b']' {
                        break;
                    }
                    if pattern[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                i = j + 1;
            }
            b'\\' if i + 1 < pattern.len() => {
                out.extend(like_escape(&pattern[i + 1..i + 2]));
                i += 2;
            }
            b => {
                out.extend(like_escape(&[b]));
                i += 1;
            }
        }
    }
    out
}

/// Literal bytes a regex anchored with `^` requires at the start: plain
/// characters up to the first metacharacter. Anything else yields an empty
/// prefix and a full streamed scan.
fn regex_prefix(source: &str) -> Vec<u8> {
    let Some(rest) = source.strip_prefix('^') else { return Vec::new() };
    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e) if !e.is_ascii_alphanumeric() => out.extend(e.to_string().as_bytes()),
                _ => break,
            },
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '$' => break,
            _ => {
                // a quantifier after this char would make it optional
                if matches!(chars.peek(), Some('*' | '?' | '{')) {
                    break;
                }
                out.extend(c.to_string().as_bytes());
            }
        }
    }
    out
}

/// Limits for a server-side verification.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    pub count: usize,
    pub gap: Duration,
    pub max_calls: u64,
    pub max_time: Duration,
    /// Matches on either side above this abort the reconciliation.
    pub max_matches: usize,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        VerifyOptions {
            count: 2000,
            gap: Duration::ZERO,
            max_calls: 20_000,
            max_time: Duration::from_secs(30),
            max_matches: 500_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub calls: u64,
    /// Keys returned by `SCAN`, duplicates included.
    pub examined: u64,
    /// Distinct keys the server matched.
    pub matched: u64,
    pub added: u64,
    pub removed: u64,
    /// The cursor came back to 0: the server side is exhaustive and
    /// index entries it did not confirm were removed.
    pub complete: bool,
    /// A limit stopped the walk or the match set grew too large; nothing
    /// was removed in that case.
    pub truncated: bool,
}

/// Walk the server with `SCAN MATCH pattern` and reconcile the index for
/// that pattern: keys the server has and the index lacks are added; keys
/// the index has, the pattern matches and the server did not return are
/// removed, but only when the walk completed.
pub async fn verify(
    router: &mut Router,
    index: &mut Index,
    pattern: &[u8],
    opts: &VerifyOptions,
    cancel: &AtomicBool,
) -> Result<VerifyReport> {
    let started = Instant::now();
    let mut report = VerifyReport::default();
    let mut server: HashSet<Vec<u8>> = HashSet::new();
    // Every primary is walked (one for a standalone server); the walk is
    // complete only when each node's cursor came back to 0.
    let mut all_complete = true;
    for addr in router.primaries() {
        if report.truncated {
            break;
        }
        let mut cursor = 0u64;
        let mut node_complete = false;
        loop {
            if cancel.load(Ordering::Relaxed) || report.calls >= opts.max_calls || started.elapsed() > opts.max_time {
                report.truncated = true;
                break;
            }
            let cmd = commands::scan(cursor, Some(pattern), opts.count, None);
            let reply = crate::routed!(router, Hop::node(&addr), |c| c.raw(&cmd).await)?;
            let (next, keys) = parse_scan_reply(reply)?;
            report.calls += 1;
            report.examined += keys.len() as u64;
            server.extend(keys);
            if server.len() > opts.max_matches {
                report.truncated = true;
                break;
            }
            cursor = next;
            if cursor == 0 {
                node_complete = true;
                break;
            }
            if !opts.gap.is_zero() {
                tokio::time::sleep(opts.gap).await;
            }
        }
        all_complete &= node_complete;
    }
    report.complete = all_complete && !report.truncated;
    report.matched = server.len() as u64;

    let plan = Plan::new(Query {
        mode: Mode::Glob,
        term: pattern.to_vec(),
    })?;
    let mut local: HashSet<Vec<u8>> = HashSet::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = index.search(&plan, after.as_deref(), crate::index::MAX_WINDOW, cancel)?;
        if page.cancelled {
            report.truncated = true;
            break;
        }
        after = page.rows.last().map(|r| r.name.clone());
        local.extend(page.rows.into_iter().map(|r| r.name));
        if local.len() > opts.max_matches {
            report.truncated = true;
            break;
        }
        if page.done {
            break;
        }
    }
    if local.len() > opts.max_matches {
        return Ok(report);
    }

    let generation = index.scan_state()?.generation;
    let added: Vec<Vec<u8>> = server.iter().filter(|k| !local.contains(*k)).cloned().collect();
    if !added.is_empty() {
        report.added = index.add(added, generation)?.new;
    }
    if report.complete && !report.truncated {
        let removed: Vec<Vec<u8>> = local.into_iter().filter(|k| !server.contains(k)).collect();
        if !removed.is_empty() {
            report.removed = index.remove(&removed)?;
        }
    }
    Ok(report)
}

/// `EXISTS key` for jump-to-key: one round trip, no index involved.
pub async fn exists(conn: &mut Conn, key: &[u8]) -> Result<bool> {
    match conn.raw(&commands::exists(&[key])).await? {
        Value::Int(n) => Ok(n > 0),
        other => Err(Error::reply("EXISTS", format!("{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_syntax() {
        assert_eq!(parse("user:42:*").mode, Mode::Glob);
        assert_eq!(parse("session").mode, Mode::Substring);
        assert_eq!(parse("/user:\\d+:cart/").mode, Mode::Regex);
        assert_eq!(parse("/user:\\d+:cart/").term, b"user:\\d+:cart");
        assert_eq!(parse("/").mode, Mode::Substring);
        assert_eq!(parse("  a?b ").term, b"a?b");
        assert_eq!(parse("user:42:profile").mode, Mode::Substring);
        assert_eq!(parse("a\\*b").mode, Mode::Substring);
    }

    #[test]
    fn a_prefix_query_matches_only_keys_under_those_bytes() {
        // glob characters in a key name are not a pattern
        let q = Query::prefix(b"a*b:");
        assert_eq!((q.mode, &q.term[..]), (Mode::Glob, &b"a\\*b:*"[..]));
        let p = Plan::new(q).unwrap();
        assert_eq!(p.lo, b"a*b:");
        assert!(p.matcher.matches(b"a*b:1") && p.matcher.matches(b"a*b:"));
        assert!(!p.matcher.matches(b"aXb:1") && !p.matcher.matches(b"a*b"));
        // the typed pattern with the same text is wider
        assert!(Plan::new(parse("a*b:*")).unwrap().matcher.matches(b"aXb:1"));

        // names that are not UTF-8, and nothing is trimmed
        let p = Plan::new(Query::prefix(b" bin:\xff\\[")).unwrap();
        assert_eq!(p.lo, b" bin:\xff\\[");
        assert!(p.matcher.matches(b" bin:\xff\\[\x00"));
        assert!(!p.matcher.matches(b"bin:\xff\\[\x00") && !p.matcher.matches(b" bin:\xfe\\[\x00"));
    }

    #[test]
    fn plans_narrow_the_scan() {
        let p = Plan::new(parse("user:42:*")).unwrap();
        assert_eq!(p.lo, b"user:42:");
        assert_eq!(p.hi.as_deref(), Some(&b"user:42;"[..]));
        assert_eq!(p.like.as_deref(), Some(&b"user:42:%"[..]));
        assert!(p.fts_literal.is_none(), "prefix-shaped globs stay on the B-tree");
        assert!(!p.like_is_exact);

        let p = Plan::new(parse("*sess_ion*")).unwrap();
        assert!(p.lo.is_empty());
        assert_eq!(p.like.as_deref(), Some(&b"%sess\\_ion%"[..]));
        assert!(p.fts_literal.is_none(), "an escaped LIKE cannot use the trigram index");
        assert_eq!(Plan::new(parse("*session*")).unwrap().fts_literal.as_deref(), Some("session"));

        let p = Plan::new(parse("u?er:[0-9][^a-z]x\\*")).unwrap();
        assert_eq!(p.like.as_deref(), Some(&b"u_er:__x*"[..]));
        assert_eq!(p.lo, b"u");

        let p = Plan::new(parse("Session")).unwrap();
        assert_eq!(p.like.as_deref(), Some(&b"%Session%"[..]));
        assert!(p.like_is_exact);
        assert!(p.matcher.matches(b"user:1:SESSION"));
        assert!(!p.matcher.matches(b"user:1:sesion"));
        assert_eq!(p.server_pattern().unwrap(), b"*Session*");
        assert_eq!(Plan::new(parse("ab")).unwrap().fts_literal, None);
        assert_eq!(Plan::new(parse("abc")).unwrap().fts_literal.as_deref(), Some("abc"));

        let p = Plan::new(parse("/^user:4\\d+:cart$/")).unwrap();
        assert_eq!(p.lo, b"user:4");
        assert!(p.matcher.matches(b"user:42:cart"));
        assert!(!p.matcher.matches(b"user:42:carts"));
        assert!(p.server_pattern().is_none());
        assert_eq!(Plan::new(parse("/^a\\.b*c/")).unwrap().lo, b"a.");
        assert_eq!(Plan::new(parse("/^ab?c/")).unwrap().lo, b"a");
        assert!(Plan::new(parse("/^(a)/")).unwrap().lo.is_empty());
        assert!(Plan::new(parse("/(/")).is_err());
        assert_eq!(Plan::new(parse("user:*:cart")).unwrap().server_pattern().unwrap(), b"user:*:cart");
    }
}
