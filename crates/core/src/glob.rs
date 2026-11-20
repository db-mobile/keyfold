//! Redis-compatible glob matching over bytes: `*`, `?`, `[abc]`, `[^abc]`,
//! `[a-z]` and `\` escapes, the same rules `SCAN MATCH` applies on the
//! server, so a pattern evaluated against the index and the same pattern
//! sent to the server agree on every key.

/// Does `pattern` match `s` under Redis glob rules (case-sensitive)?
pub fn matches(pattern: &[u8], s: &[u8]) -> bool {
    match_at(pattern, s, 0)
}

fn match_at(mut p: &[u8], mut s: &[u8], depth: u32) -> bool {
    // Guard against pathological nesting the same way the server does.
    if depth > 1000 {
        return false;
    }
    while !p.is_empty() {
        match p[0] {
            b'*' => {
                // Collapse runs of stars; a trailing star matches the rest.
                while p.len() > 1 && p[1] == b'*' {
                    p = &p[1..];
                }
                if p.len() == 1 {
                    return true;
                }
                let rest = &p[1..];
                for i in 0..=s.len() {
                    if match_at(rest, &s[i..], depth + 1) {
                        return true;
                    }
                }
                return false;
            }
            b'?' => {
                if s.is_empty() {
                    return false;
                }
                s = &s[1..];
                p = &p[1..];
            }
            b'[' => {
                let Some((matched, consumed)) = match_class(&p[1..], s.first().copied()) else {
                    return false;
                };
                if !matched {
                    return false;
                }
                s = &s[1..];
                p = &p[1 + consumed..];
            }
            b'\\' if p.len() > 1 => {
                if s.is_empty() || s[0] != p[1] {
                    return false;
                }
                s = &s[1..];
                p = &p[2..];
            }
            c => {
                if s.is_empty() || s[0] != c {
                    return false;
                }
                s = &s[1..];
                p = &p[1..];
            }
        }
    }
    s.is_empty()
}

/// Evaluate a character class whose body starts right after `[`. Returns
/// whether `c` is in it and how many pattern bytes the class occupies
/// (including the closing `]`); `None` when there is no byte to test.
/// An unterminated class runs to the end of the pattern, as on the server.
fn match_class(body: &[u8], c: Option<u8>) -> Option<(bool, usize)> {
    let c = c?;
    let mut i = 0;
    let mut negate = false;
    if body.first() == Some(&b'^') {
        negate = true;
        i = 1;
    }
    let mut found = false;
    let consumed = loop {
        match body.get(i) {
            None => break body.len(),
            Some(b']') => break i + 1,
            Some(&b'\\') if i + 1 < body.len() => {
                if body[i + 1] == c {
                    found = true;
                }
                i += 2;
            }
            Some(&lo) if body.get(i + 1) == Some(&b'-') && i + 2 < body.len() => {
                let hi = body[i + 2];
                let (lo, hi) = if lo > hi { (hi, lo) } else { (lo, hi) };
                if lo <= c && c <= hi {
                    found = true;
                }
                i += 3;
            }
            Some(&b) => {
                if b == c {
                    found = true;
                }
                i += 1;
            }
        }
    };
    Some((found != negate, consumed))
}

/// Is any byte in `pattern` a glob metacharacter (unescaped)?
pub fn has_meta(pattern: &[u8]) -> bool {
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'\\' => i += 2,
            b'*' | b'?' | b'[' => return true,
            _ => i += 1,
        }
    }
    false
}

/// The literal bytes every match must start with: the pattern up to its
/// first metacharacter, escapes resolved. For `user:42:*` that is
/// `user:42:`; for `*session*` it is empty.
pub fn literal_prefix(pattern: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'*' | b'?' | b'[' => break,
            b'\\' if i + 1 < pattern.len() => {
                out.push(pattern[i + 1]);
                i += 2;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// The longest run of literal bytes anywhere in the pattern, escapes
/// resolved: the best term for a trigram prefilter.
pub fn longest_literal(pattern: &[u8]) -> Vec<u8> {
    let mut best = Vec::new();
    let mut cur = Vec::new();
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'*' | b'?' => {
                if cur.len() > best.len() {
                    best = std::mem::take(&mut cur);
                } else {
                    cur.clear();
                }
                i += 1;
            }
            b'[' => {
                if cur.len() > best.len() {
                    best = std::mem::take(&mut cur);
                } else {
                    cur.clear();
                }
                // skip the class
                let (_, consumed) = match_class(&pattern[i + 1..], Some(0)).unwrap_or((false, 0));
                i += 1 + consumed;
            }
            b'\\' if i + 1 < pattern.len() => {
                cur.push(pattern[i + 1]);
                i += 2;
            }
            b => {
                cur.push(b);
                i += 1;
            }
        }
    }
    if cur.len() > best.len() { cur } else { best }
}

/// Escape a literal so that it matches itself as a glob (for `SCAN MATCH`
/// of a substring search: `*term*`).
pub fn escape(literal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(literal.len());
    for &b in literal {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// The smallest byte string greater than every string with this prefix,
/// or `None` when the prefix is empty or all `0xff`.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(&last) = end.last() {
        if last == 0xff {
            end.pop();
        } else {
            *end.last_mut().unwrap() += 1;
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_like_the_server() {
        let yes = [
            ("*", ""),
            ("*", "anything"),
            ("user:*", "user:42"),
            ("user:*", "user:"),
            ("user:?:cart", "user:4:cart"),
            ("user:[0-9]*", "user:42"),
            ("user:[^a-z]*", "user:42"),
            ("h[ae]llo", "hello"),
            ("h[ae]llo", "hallo"),
            ("h[^e]llo", "hallo"),
            ("a\\*b", "a*b"),
            ("a\\[b", "a[b"),
            ("*:cart", "user:1:cart"),
            ("*a*b*c*", "xaxbxc"),
            ("[a-", "a"),
            ("[a-", "-"),
            ("[^", "x"),
            ("[\\]]", "]"),
            ("**x", "abcx"),
        ];
        for (p, s) in yes {
            assert!(matches(p.as_bytes(), s.as_bytes()), "{p:?} should match {s:?}");
        }
        let no = [
            ("user:*", "usr:42"),
            ("user:?:cart", "user:42:cart"),
            ("user:[0-9]*", "user:x"),
            ("h[^e]llo", "hello"),
            ("a\\*b", "ab"),
            ("*:cart", "user:1:cart:x"),
            ("abc", "abcd"),
            ("abcd", "abc"),
            ("", "a"),
            ("?", ""),
            ("[a-", "[a-"),
            ("a[", "a["),
            ("a[", "a"),
            ("[]]", "]"),
        ];
        for (p, s) in no {
            assert!(!matches(p.as_bytes(), s.as_bytes()), "{p:?} should not match {s:?}");
        }
        assert!(matches(b"", b""));
    }

    #[test]
    fn prefixes_and_literals() {
        assert_eq!(literal_prefix(b"user:42:*"), b"user:42:");
        assert_eq!(literal_prefix(b"*session*"), b"");
        assert_eq!(literal_prefix(b"a\\*b*"), b"a*b");
        assert_eq!(literal_prefix(b"plain"), b"plain");
        assert!(has_meta(b"a*"));
        assert!(has_meta(b"a[b]"));
        assert!(!has_meta(b"a\\*b"));
        assert!(!has_meta(b"plain"));
        assert_eq!(longest_literal(b"*session*"), b"session");
        assert_eq!(longest_literal(b"user:*:cart"), b"user:");
        assert_eq!(longest_literal(b"u*profile[0-9]x"), b"profile");
        assert_eq!(longest_literal(b"a?b"), b"a");
        assert_eq!(escape(b"a*b[c]"), b"a\\*b\\[c\\]");
        assert!(matches(&escape(b"a*b[c]\\"), b"a*b[c]\\"));
        assert_eq!(prefix_end(b"user:"), Some(b"user;".to_vec()));
        assert_eq!(prefix_end(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xff\xff"), None);
        assert_eq!(prefix_end(b""), None);
    }
}
