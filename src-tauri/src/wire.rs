//! How byte strings cross the IPC. Redis names are binary safe, JSON
//! strings are not: a byte string that is valid UTF-8 travels as `text`
//! alone; any other travels as a display `text` plus its exact bytes as
//! lowercase hex in `raw`, and the webview hands `raw` back unchanged.

use serde::{Deserialize, Serialize};

/// A key name on its way to the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyDto {
    pub text: String,
    pub raw: Option<String>,
}

impl KeyDto {
    pub fn new(bytes: &[u8]) -> KeyDto {
        let (text, raw) = display_and_raw(bytes);
        KeyDto { text, raw }
    }
}

/// A key name coming back from the webview; `raw` wins when present.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyArg {
    pub text: String,
    #[serde(default)]
    pub raw: Option<String>,
}

impl KeyArg {
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        bytes_from(&self.text, self.raw.as_deref())
    }
}

/// Text and exact bytes for values and collection entries: lossy text when
/// the bytes are not UTF-8.
pub fn text_and_raw(bytes: &[u8]) -> (String, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), None),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), Some(hex(bytes))),
    }
}

/// Text and exact bytes for key names, segments and prefixes. A name that
/// is not UTF-8 is shown escaped, so that two different names never look
/// the same: valid runs as they are, every other byte as `\xNN`, and a
/// literal backslash doubled. A UTF-8 name is never escaped.
pub fn display_and_raw(bytes: &[u8]) -> (String, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), None),
        Err(_) => (escaped(bytes), Some(hex(bytes))),
    }
}

fn escaped(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 8);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

pub fn bytes_from(text: &str, raw: Option<&str>) -> Result<Vec<u8>, String> {
    match raw {
        Some(h) => unhex(h),
        None => Ok(text.as_bytes().to_vec()),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let digits = s.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    let nibble = |d: u8| match d {
        b'0'..=b'9' => Ok(d - b'0'),
        b'a'..=b'f' => Ok(d - b'a' + 10),
        b'A'..=b'F' => Ok(d - b'A' + 10),
        _ => Err(format!("not a hex digit: {:?}", d as char)),
    };
    digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Ok(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_names_travel_as_text_alone() {
        assert_eq!(display_and_raw(b"user:1"), ("user:1".into(), None));
        assert_eq!(display_and_raw("k\u{e4}se:\\x41".as_bytes()), ("k\u{e4}se:\\x41".into(), None));
        // NUL is valid UTF-8 and survives a JSON string
        assert_eq!(display_and_raw(b"a\x00b"), ("a\0b".into(), None));
    }

    #[test]
    fn binary_names_are_escaped_and_carry_their_bytes() {
        let (text, raw) = display_and_raw(b"bin:\xff\xfe\x00z");
        assert_eq!(text, "bin:\\xff\\xfe\0z");
        assert_eq!(raw.as_deref(), Some("62696e3afffe007a"));
        // distinct names have distinct texts
        assert_ne!(display_and_raw(b"a\xff").0, display_and_raw(b"a\xfe").0);
        // a literal backslash cannot be mistaken for an escape
        assert_eq!(display_and_raw(b"\\xff\xff").0, "\\\\xff\\xff");
        // valid multi-byte runs stay readable, a truncated one is escaped
        assert_eq!(display_and_raw(b"\xc3\xa4\xc3").0, "\u{e4}\\xc3");
    }

    #[test]
    fn every_byte_string_round_trips() {
        let all: Vec<u8> = (0..=255u8).collect();
        for bytes in [&all[..], b"", b"plain", b"\xff", b"a:\x80:b", "\u{1f600}".as_bytes()] {
            let k = KeyDto::new(bytes);
            let back = KeyArg { text: k.text, raw: k.raw };
            assert_eq!(back.bytes().unwrap(), bytes);
        }
    }

    #[test]
    fn raw_wins_over_text() {
        let k = KeyArg {
            text: "shown".into(),
            raw: Some("00ff".into()),
        };
        assert_eq!(k.bytes().unwrap(), b"\x00\xff");
    }

    #[test]
    fn unhex_rejects_what_is_not_hex() {
        assert_eq!(unhex("").unwrap(), b"");
        assert_eq!(unhex("0aFf").unwrap(), b"\x0a\xff");
        assert!(unhex("abc").is_err());
        assert!(unhex("zz").is_err());
        assert!(unhex("+1").is_err());
        // multi-byte input used to slice inside a character
        assert!(unhex("\u{e4}").is_err());
        assert!(unhex("a\u{e4}b").is_err());
    }
}
