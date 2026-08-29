//! Minimal JSON — value model, serializer, recursive-descent parser.
//!
//! Pure std, written for the MCP stdio transport where every message is
//! one newline-delimited JSON document. Supports the full JSON data
//! model: object / array / string (with `\uXXXX` escapes and surrogate
//! pairs) / number (i64 fast path, f64 otherwise) / bool / null.

use std::fmt::Write as _;

/// A JSON value. Objects preserve insertion order (a Vec of pairs), which
/// keeps serialization deterministic for tests and diffs.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// Integral number (no `.`/exponent on the wire, fits i64).
    Int(i64),
    /// Any other number. Must be finite — JSON has no inf/nan; producers
    /// (see `tools::reply_to_json`) map non-finite doubles to strings.
    Float(f64),
    /// String.
    Str(String),
    /// Array.
    Array(Vec<Value>),
    /// Object as ordered key/value pairs.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Object field lookup (first match); `None` on non-objects.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Borrow the string payload, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow the items, if this is an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Serialize to a single-line JSON string.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    /// Appends this value's JSON text to `out`.
    ///
    /// Recursive, matching the parser: MCP frames nest a handful of levels,
    /// and a document deep enough to overflow the stack here would have
    /// failed to parse on the way in.
    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) => {
                let _ = write!(out, "{f}");
            }
            Value::Str(s) => write_escaped(s, out),
            Value::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
            Value::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// Shorthand: string [`Value`] from a `&str`.
pub fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

/// Shorthand: object [`Value`] from `(&str, Value)` pairs.
pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Writes `text` as a quoted JSON string, escaping what JSON requires.
///
/// Every code point below 0x20 is escaped, the six with short forms by
/// name and the rest as `\uXXXX`. Non-ASCII is emitted literally: the
/// transport is UTF-8, and escaping it would only make frames larger and
/// harder to read in a log.
fn write_escaped(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse one complete JSON document; trailing non-whitespace is an error.
pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser { s: input, pos: 0 };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos != p.s.len() {
        return Err(p.err("trailing characters after JSON document"));
    }
    Ok(v)
}

/// A cursor over one document's text.
struct Parser<'a> {
    /// The whole input; borrowed, so string slices can come straight out
    /// of it when no escape needs decoding.
    s: &'a str,
    /// Byte offset of the next character to read. Every error message
    /// carries it, which is what makes a malformed frame diagnosable.
    pos: usize,
}

impl<'a> Parser<'a> {
    /// An error message with the byte offset where parsing stopped.
    fn err(&self, msg: &str) -> String {
        format!("{msg} at byte {}", self.pos)
    }

    /// The next byte without consuming it; `None` at end of input.
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.pos).copied()
    }

    /// Consumes and returns the next byte.
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// Advances past JSON's four whitespace bytes.
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Consumes `want`, or reports which byte was expected and where.
    fn expect(&mut self, want: u8) -> Result<(), String> {
        if self.peek() == Some(want) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", want as char)))
        }
    }

    /// Slice `[a, b)` of the input. Both bounds sit on ASCII delimiters
    /// (`"` / `\`) or at run starts, so they are char boundaries; `get`
    /// still verifies rather than panicking.
    fn slice(&self, a: usize, b: usize) -> Result<&'a str, String> {
        self.s
            .get(a..b)
            .ok_or_else(|| self.err("invalid utf-8 boundary"))
    }

    /// One value of any kind, dispatched on its first byte.
    ///
    /// JSON is prefix-distinguishable, so a single byte of lookahead
    /// decides the production — no backtracking anywhere in this parser.
    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(self.err(&format!("unexpected character '{}'", c as char))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    /// One of the three bare words, or an error.
    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.s[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    /// `{ … }`, keeping pairs in the order they appeared.
    ///
    /// Duplicate keys are kept rather than merged: [`Value::get`] returns
    /// the first, which is what a reader of the raw text would take.
    fn object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => return Ok(Value::Object(pairs)),
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    /// `[ … ]`.
    fn array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => return Ok(Value::Array(items)),
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    /// A quoted string with its escapes decoded.
    ///
    /// Copies in runs between escapes rather than character by character,
    /// so a string with no escapes — nearly all of them — is one
    /// `push_str` of a borrowed slice. A raw control character is
    /// rejected, as JSON requires.
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        let mut run = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    out.push_str(self.slice(run, self.pos)?);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(self.slice(run, self.pos)?);
                    self.pos += 1;
                    self.escape(&mut out)?;
                    run = self.pos;
                }
                Some(c) if c < 0x20 => {
                    return Err(self.err("raw control character in string"));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// One escape sequence, after the backslash has been consumed.
    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        match self.bump() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{08}'),
            Some(b'f') => out.push('\u{0c}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => out.push(self.unicode_escape()?),
            _ => return Err(self.err("invalid escape sequence")),
        }
        Ok(())
    }

    /// `\uXXXX` after the `\u` marker, including UTF-16 surrogate pairs
    /// (`😀` → 😀). Unpaired surrogates are rejected.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        let code = match hi {
            0xD800..=0xDBFF => {
                if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                    return Err(self.err("high surrogate not followed by \\u escape"));
                }
                let lo = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(self.err("invalid low surrogate"));
                }
                0x1_0000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(self.err("unpaired low surrogate")),
            c => u32::from(c),
        };
        char::from_u32(code).ok_or_else(|| self.err("invalid unicode scalar"))
    }

    /// Exactly four hex digits, as read by `\u`.
    ///
    /// A truncated or non-hex run is an error rather than a short read:
    /// accepting `\u12"` would silently change the string's contents.
    fn hex4(&mut self) -> Result<u16, String> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let c = self.bump().ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = (c as char)
                .to_digit(16)
                .ok_or_else(|| self.err("bad hex digit in \\u escape"))?;
            v = (v << 4) | d as u16;
        }
        Ok(v)
    }

    /// A number, as [`Value::Int`] when it fits and [`Value::Float`] otherwise.
    ///
    /// The integral form is tried first so an id round-trips exactly — a
    /// JSON-RPC id echoed back as `3.0` instead of `3` is a different id
    /// to the host that sent it.
    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.pos += 1;
        }
        let text = self.slice(start, self.pos)?;
        if text.bytes().any(|c| matches!(c, b'.' | b'e' | b'E')) {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| self.err("invalid number"))
        } else {
            match text.parse::<i64>() {
                Ok(n) => Ok(Value::Int(n)),
                // Integral but beyond i64: JSON allows arbitrary precision.
                Err(_) => text
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| self.err("invalid number")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_nested_document() {
        let v = obj(vec![
            ("name", s("kevy")),
            ("n", Value::Int(-42)),
            ("pi", Value::Float(3.25)),
            ("ok", Value::Bool(true)),
            ("gone", Value::Null),
            (
                "items",
                Value::Array(vec![s("a"), Value::Int(1), Value::Object(Vec::new())]),
            ),
        ]);
        let text = v.serialize();
        assert_eq!(parse(&text).unwrap(), v);
    }

    #[test]
    fn escapes_roundtrip() {
        let v = s("quote\" back\\ nl\n tab\t ctrl\u{01} 中文");
        let text = v.serialize();
        assert!(text.contains("\\\""));
        assert!(text.contains("\\u0001"));
        assert_eq!(parse(&text).unwrap(), v);
    }

    #[test]
    fn unicode_escapes_parse() {
        assert_eq!(parse(r#""A""#).unwrap(), s("A"));
        // Surrogate pair → 😀 (U+1F600).
        assert_eq!(parse(r#""😀""#).unwrap(), s("😀"));
        assert!(parse(r#""\uDE00""#).is_err()); // lone low surrogate
        assert!(parse(r#""\uD83Dx""#).is_err()); // unpaired high surrogate
    }

    #[test]
    fn numbers_parse() {
        assert_eq!(parse("0").unwrap(), Value::Int(0));
        assert_eq!(parse("-7").unwrap(), Value::Int(-7));
        assert_eq!(parse("1.5").unwrap(), Value::Float(1.5));
        assert_eq!(parse("2e3").unwrap(), Value::Float(2000.0));
        // Integral beyond i64 falls back to f64.
        assert!(matches!(
            parse("99999999999999999999").unwrap(),
            Value::Float(_)
        ));
    }

    #[test]
    fn malformed_documents_rejected() {
        for bad in [
            "",
            "{",
            "[1,",
            r#"{"a" 1}"#,
            r#""unterminated"#,
            "tru",
            "1 2",
            "\"raw\u{01}\"",
            r#"{"a":1,}"#,
        ] {
            assert!(parse(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn object_get_and_accessors() {
        let v = parse(r#"{"a":"x","b":[1,2]}"#).unwrap();
        assert_eq!(v.get("a").and_then(Value::as_str), Some("x"));
        assert_eq!(v.get("b").and_then(Value::as_array).map(<[Value]>::len), Some(2));
        assert!(v.get("missing").is_none());
    }
}
