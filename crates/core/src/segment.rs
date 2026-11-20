//! Key name splitting and segment shape classification (CONCEPT section 5.6).
//! Everything here is pure and byte oriented: Redis keys are binary safe.

/// Shape class of one path segment. The order of the variants is the order
/// in which the classes are tested; the first match wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SegClass {
    Uuid = 0,
    Ulid = 1,
    Hex = 2,
    Numeric = 3,
    Date = 4,
    Opaque = 5,
    Email = 6,
    Literal = 7,
}

impl SegClass {
    pub const ALL: [SegClass; 8] = [
        SegClass::Uuid,
        SegClass::Ulid,
        SegClass::Hex,
        SegClass::Numeric,
        SegClass::Date,
        SegClass::Opaque,
        SegClass::Email,
        SegClass::Literal,
    ];

    /// Column name of the per-class counter in `nodes`.
    pub fn column(self) -> &'static str {
        match self {
            SegClass::Uuid => "c_uuid",
            SegClass::Ulid => "c_ulid",
            SegClass::Hex => "c_hex",
            SegClass::Numeric => "c_num",
            SegClass::Date => "c_date",
            SegClass::Opaque => "c_opaque",
            SegClass::Email => "c_email",
            SegClass::Literal => "c_literal",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Classify a single segment into the first matching shape class.
pub fn classify(seg: &[u8]) -> SegClass {
    if is_uuid(seg) {
        SegClass::Uuid
    } else if is_ulid(seg) {
        SegClass::Ulid
    } else if is_hex_id(seg) {
        SegClass::Hex
    } else if is_numeric(seg) {
        SegClass::Numeric
    } else if is_date(seg) {
        SegClass::Date
    } else if is_opaque(seg) {
        SegClass::Opaque
    } else if is_email(seg) {
        SegClass::Email
    } else {
        SegClass::Literal
    }
}

/// Numeric value of an all-digit segment that fits in an i64, for numeric
/// sorting and range buckets. Longer digit strings return `None`.
pub fn numeric_value(seg: &[u8]) -> Option<i64> {
    if seg.is_empty() || seg.len() > 18 || !seg.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v: i64 = 0;
    for &b in seg {
        v = v * 10 + i64::from(b - b'0');
    }
    Some(v)
}

fn is_uuid(s: &[u8]) -> bool {
    s.len() == 36
        && s.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn is_crockford(b: u8) -> bool {
    matches!(b.to_ascii_uppercase(), b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

fn is_ulid(s: &[u8]) -> bool {
    s.len() == 26 && s.iter().all(|&b| is_crockford(b))
}

fn is_hex_id(s: &[u8]) -> bool {
    matches!(s.len(), 16 | 24 | 32 | 40 | 64) && s.iter().all(u8::is_ascii_hexdigit)
}

fn is_numeric(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(u8::is_ascii_digit)
}

fn is_date(s: &[u8]) -> bool {
    if s.len() < 10 {
        return false;
    }
    let d = |i: usize| s[i].is_ascii_digit();
    if !(d(0) && d(1) && d(2) && d(3) && s[4] == b'-' && d(5) && d(6) && s[7] == b'-' && d(8) && d(9)) {
        return false;
    }
    let month = (s[5] - b'0') * 10 + (s[6] - b'0');
    let day = (s[8] - b'0') * 10 + (s[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

fn is_opaque(s: &[u8]) -> bool {
    if s.len() < 8 {
        return false;
    }
    let mut lower = false;
    let mut upper = false;
    let mut digit = false;
    for &b in s {
        match b {
            b'a'..=b'z' => lower = true,
            b'A'..=b'Z' => upper = true,
            b'0'..=b'9' => digit = true,
            b'-' | b'_' => {}
            _ => return false,
        }
    }
    digit || (lower && upper)
}

fn is_email(s: &[u8]) -> bool {
    match s.iter().position(|&b| b == b'@') {
        Some(at) => at > 0 && s[at + 1..].contains(&b'.'),
        None => false,
    }
}

/// Split a key into its path segments on a single-byte delimiter. A key with
/// no delimiter is one segment. Empty segments are kept so that `a::b` and
/// `a:b` stay distinct paths.
pub fn split(key: &[u8], delimiter: u8) -> impl Iterator<Item = &[u8]> {
    key.split(move |&b| b == delimiter)
}

/// Candidate delimiters, in tie-break order.
pub const DELIMITER_CANDIDATES: [u8; 5] = *b":/._|";

/// Pick the delimiter that occurs in the most keys of a sample. Returns `:`
/// when no candidate occurs at all.
pub fn detect_delimiter<'a>(keys: impl IntoIterator<Item = &'a [u8]>) -> u8 {
    let mut hits = [0usize; DELIMITER_CANDIDATES.len()];
    for k in keys {
        for (i, &d) in DELIMITER_CANDIDATES.iter().enumerate() {
            if k.contains(&d) {
                hits[i] += 1;
            }
        }
    }
    let mut best = 0;
    for i in 1..hits.len() {
        if hits[i] > hits[best] {
            best = i;
        }
    }
    DELIMITER_CANDIDATES[best]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_shapes_in_table_order() {
        assert_eq!(classify(b"550e8400-e29b-41d4-a716-446655440000"), SegClass::Uuid);
        assert_eq!(classify(b"01ARZ3NDEKTSV4RRFFQ69G5FAV"), SegClass::Ulid);
        assert_eq!(classify(b"9f86d081884c7d659a2feaa0c55ad015"), SegClass::Hex);
        assert_eq!(classify(b"0123456789abcdef"), SegClass::Hex);
        assert_eq!(classify(b"4821"), SegClass::Numeric);
        assert_eq!(classify(b"1700000000"), SegClass::Numeric);
        assert_eq!(classify(b"2024-03-15"), SegClass::Date);
        assert_eq!(classify(b"2024-03-15T10:00"), SegClass::Date);
        assert_eq!(classify(b"2024-13-01"), SegClass::Opaque); // invalid month, falls through to opaque
        assert_eq!(classify(b"sess_A1b2C3d4"), SegClass::Opaque);
        assert_eq!(classify(b"abcdefgh1"), SegClass::Opaque);
        assert_eq!(classify(b"abcdefgh"), SegClass::Literal);
        assert_eq!(classify(b"jane.doe@example.com"), SegClass::Email);
        assert_eq!(classify(b"profile"), SegClass::Literal);
        assert_eq!(classify(b""), SegClass::Literal);
        assert_eq!(classify(b"@foo"), SegClass::Literal);
    }

    #[test]
    fn numeric_value_limits() {
        assert_eq!(numeric_value(b"42"), Some(42));
        assert_eq!(numeric_value(b"000"), Some(0));
        assert_eq!(numeric_value(b"123456789012345678"), Some(123456789012345678));
        assert_eq!(numeric_value(b"1234567890123456789"), None);
        assert_eq!(numeric_value(b"4a"), None);
        assert_eq!(numeric_value(b""), None);
    }

    #[test]
    fn split_keeps_empty_segments() {
        let v: Vec<&[u8]> = split(b"a::b", b':').collect();
        assert_eq!(v, vec![&b"a"[..], &b""[..], &b"b"[..]]);
        let v: Vec<&[u8]> = split(b"plain", b':').collect();
        assert_eq!(v, vec![&b"plain"[..]]);
        let v: Vec<&[u8]> = split(b"trail:", b':').collect();
        assert_eq!(v, vec![&b"trail"[..], &b""[..]]);
    }

    #[test]
    fn delimiter_detection_prefers_most_common() {
        let keys: Vec<&[u8]> = vec![b"a/b/c", b"x/y", b"p:q", b"noop"];
        assert_eq!(detect_delimiter(keys), b'/');
        let none: Vec<&[u8]> = vec![b"abc", b"def"];
        assert_eq!(detect_delimiter(none), b':');
        let tie: Vec<&[u8]> = vec![b"a:b/c"];
        assert_eq!(detect_delimiter(tie), b':');
    }
}
