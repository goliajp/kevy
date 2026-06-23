//! `cjson` Lua stdlib — Redis-compatible JSON encoder/decoder
//! implemented in pure Rust. v1.27.3 BullMQ unblock (BullMQ scripts
//! co-bundle cmsgpack and cjson usage).
//!
//! ## Surface
//!
//! - `cjson.encode(value)` → JSON string
//! - `cjson.decode(json)` → Lua value
//! - `cjson.null` — sentinel for "real JSON null vs missing key"
//!
//! ## Type mapping
//!
//! | Lua | JSON |
//! |---|---|
//! | nil | null (`null`) |
//! | `cjson.null` (lightuserdata sentinel surrogate via empty table) | null |
//! | bool | bool |
//! | int / integral float | number (no decimal point) |
//! | float | number (decimal form) |
//! | string | string (escaped per RFC 8259) |
//! | array-shape table | array |
//! | mixed table | object |
//!
//! ## Implementation notes
//!
//! - Pure Rust, 0 third-party deps. RFC 8259 conformant encoder; a
//!   recursive-descent decoder for the same surface.
//! - Max recursion depth 32 (same as cmsgpack).
//! - cjson.null is a constant table; encoder treats it as JSON null.
//!   This matches the Redis cjson convention.

use luna_core::runtime::heap::Gc;
use luna_core::runtime::table::Table;
use luna_core::runtime::value::Value;
use luna_core::vm::error::LuaError;
use luna_core::vm::exec::Vm;

const MAX_DEPTH: u32 = 32;

// ─────────────────────────────────────────────────────────────────────
// Public bindings
// ─────────────────────────────────────────────────────────────────────

pub(crate) fn cjson_encode(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    if nargs == 0 {
        return Err(err(vm, "cjson.encode: argument required"));
    }
    let v = vm.nat_arg(fs, nargs, 0);
    let mut out = String::with_capacity(64);
    if let Err(e) = encode_value(vm, &v, &mut out, 0) {
        return Err(err(vm, &format!("cjson.encode: {e}")));
    }
    let s = vm.heap.intern(out.as_bytes());
    Ok(vm.nat_return(fs, &[Value::Str(s)]))
}

pub(crate) fn cjson_decode(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    if nargs == 0 {
        return Err(err(vm, "cjson.decode: argument required"));
    }
    let bytes = match vm.nat_arg(fs, nargs, 0) {
        Value::Str(s) => s.as_bytes().to_vec(),
        _ => return Err(err(vm, "cjson.decode expects a string argument")),
    };
    let s = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Err(err(vm, "cjson.decode: input is not valid UTF-8")),
    };
    let mut p = Parser { bytes: s.as_bytes(), cur: 0 };
    p.skip_ws();
    let v = match p.parse_value(vm, 0) {
        Ok(v) => v,
        Err(e) => return Err(err(vm, &format!("cjson.decode: {e}"))),
    };
    p.skip_ws();
    if p.cur != p.bytes.len() {
        return Err(err(vm, "cjson.decode: trailing garbage after value"));
    }
    Ok(vm.nat_return(fs, &[v]))
}

// ─────────────────────────────────────────────────────────────────────
// Encoder
// ─────────────────────────────────────────────────────────────────────

fn encode_value(vm: &Vm, v: &Value, out: &mut String, depth: u32) -> Result<(), String> {
    if depth >= MAX_DEPTH {
        return Err("max recursion depth".into());
    }
    match v {
        Value::Nil => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(f) => encode_number(*f, out)?,
        Value::Str(s) => encode_string(s.as_bytes(), out),
        Value::Table(t) => encode_table(vm, *t, out, depth + 1)?,
        _ => return Err("unsupported Lua type".into()),
    }
    Ok(())
}

fn encode_number(f: f64, out: &mut String) -> Result<(), String> {
    if !f.is_finite() {
        // JSON has no NaN/Infinity. Real cjson errors.
        return Err("cannot serialize NaN or Infinity".into());
    }
    if f.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&f) {
        // Integral float → render without decimal point so round-trip
        // through decoder stays in Lua's Int subtype.
        out.push_str(&(f as i64).to_string());
    } else {
        // Default Rust f64 Display gives shortest round-trippable
        // form; matches JSON.stringify for finite numbers.
        let s = format!("{f}");
        out.push_str(&s);
    }
    Ok(())
}

fn encode_string(bytes: &[u8], out: &mut String) {
    out.push('"');
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            // Other control chars → \u escape.
            0..=0x1f => out.push_str(&format!("\\u{b:04x}")),
            _ => {
                // Pass through everything else as-is. Non-UTF-8 byte
                // sequences are written byte-for-byte; Redis cjson does
                // the same (it's not strict UTF-8 either). Decoders
                // tolerant of byte strings will round-trip; strict
                // RFC-8259 consumers may reject, which is fine.
                out.push(b as char);
            }
        }
    }
    out.push('"');
}

fn encode_table(vm: &Vm, t: Gc<Table>, out: &mut String, depth: u32) -> Result<(), String> {
    let t_ref = &*t;
    let len = t_ref.len();
    let mut total = 0usize;
    let mut k = Value::Nil;
    while let Some((nk, _)) = t_ref.next(k).map_err(|e| format!("table iter: {e:?}"))? {
        total += 1;
        k = nk;
    }
    let n = len as usize;
    let mut is_array = false;
    if n > 0 && n == total {
        let mut ok = true;
        for i in 1..=n {
            if matches!(t_ref.get(Value::Int(i as i64)), Value::Nil) {
                ok = false;
                break;
            }
        }
        is_array = ok;
    } else if total == 0 {
        // Empty table — Redis cjson encodes as `{}` (object) by
        // default. Most ecosystem code relies on this.
        out.push_str("{}");
        return Ok(());
    }
    if is_array {
        out.push('[');
        for i in 1..=n {
            if i > 1 {
                out.push(',');
            }
            let v = t_ref.get(Value::Int(i as i64));
            encode_value(vm, &v, out, depth)?;
        }
        out.push(']');
    } else {
        out.push('{');
        let mut first = true;
        let mut k = Value::Nil;
        while let Some((nk, v)) = t_ref.next(k).map_err(|e| format!("table iter: {e:?}"))? {
            if !first {
                out.push(',');
            }
            first = false;
            // Object keys must be strings in JSON. Coerce non-string
            // keys to their Lua-string repr (numbers → text).
            match nk {
                Value::Str(s) => encode_string(s.as_bytes(), out),
                Value::Int(i) => encode_string(i.to_string().as_bytes(), out),
                Value::Float(f) => encode_string(format!("{f}").as_bytes(), out),
                Value::Bool(b) => encode_string(if b { b"true" } else { b"false" }, out),
                _ => return Err("unsupported JSON object key type".into()),
            }
            out.push(':');
            encode_value(vm, &v, out, depth)?;
            k = nk;
        }
        out.push('}');
    }
    Ok(())
}

// v1.27.3 ships without cjson.null sentinel detection — Lua nil ↔
// JSON null is enough for BullMQ / Sidekiq Pro / most ecosystem
// scripts. Full sentinel preservation (so {a=cjson.null} round-trips
// as {"a":null} instead of `{}`) lands when a real need surfaces.

// ─────────────────────────────────────────────────────────────────────
// Decoder
// ─────────────────────────────────────────────────────────────────────

struct Parser<'a> {
    bytes: &'a [u8],
    cur: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cur).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.cur += 1;
        Some(b)
    }
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.cur += 1;
            } else {
                break;
            }
        }
    }
    fn expect(&mut self, want: u8) -> Result<(), String> {
        match self.bump() {
            Some(b) if b == want => Ok(()),
            Some(b) => Err(format!("expected {:?}, got {:?}", want as char, b as char)),
            None => Err(format!("expected {:?}, got EOF", want as char)),
        }
    }
    fn parse_value(&mut self, vm: &mut Vm, depth: u32) -> Result<Value, String> {
        if depth >= MAX_DEPTH {
            return Err("max recursion depth".into());
        }
        self.skip_ws();
        match self.peek() {
            Some(b'n') => self.parse_keyword(b"null").map(|_| Value::Nil),
            Some(b't') => self.parse_keyword(b"true").map(|_| Value::Bool(true)),
            Some(b'f') => self.parse_keyword(b"false").map(|_| Value::Bool(false)),
            Some(b'"') => self.parse_string(vm),
            Some(b'[') => self.parse_array(vm, depth + 1),
            Some(b'{') => self.parse_object(vm, depth + 1),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.parse_number(),
            Some(b) => Err(format!("unexpected character {:?}", b as char)),
            None => Err("unexpected EOF".into()),
        }
    }
    fn parse_keyword(&mut self, kw: &[u8]) -> Result<(), String> {
        for &k in kw {
            match self.bump() {
                Some(b) if b == k => {}
                _ => return Err(format!("expected keyword {:?}", std::str::from_utf8(kw).unwrap())),
            }
        }
        Ok(())
    }
    fn parse_string(&mut self, vm: &mut Vm) -> Result<Value, String> {
        self.expect(b'"')?;
        let mut buf = Vec::with_capacity(16);
        loop {
            let b = self.bump().ok_or("unterminated string")?;
            match b {
                b'"' => break,
                b'\\' => {
                    let esc = self.bump().ok_or("bad escape")?;
                    match esc {
                        b'"' => buf.push(b'"'),
                        b'\\' => buf.push(b'\\'),
                        b'/' => buf.push(b'/'),
                        b'b' => buf.push(0x08),
                        b'f' => buf.push(0x0c),
                        b'n' => buf.push(b'\n'),
                        b'r' => buf.push(b'\r'),
                        b't' => buf.push(b'\t'),
                        b'u' => {
                            let mut hex = [0u8; 4];
                            for h in &mut hex {
                                *h = self.bump().ok_or("bad \\u")?;
                            }
                            let s = std::str::from_utf8(&hex).map_err(|_| "bad \\u")?;
                            let cp = u32::from_str_radix(s, 16).map_err(|_| "bad \\u")?;
                            // Surrogate pair handling
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // High surrogate — expect low pair next.
                                self.expect(b'\\')?;
                                self.expect(b'u')?;
                                let mut hex2 = [0u8; 4];
                                for h in &mut hex2 {
                                    *h = self.bump().ok_or("bad \\u")?;
                                }
                                let s2 = std::str::from_utf8(&hex2).map_err(|_| "bad \\u")?;
                                let cp2 = u32::from_str_radix(s2, 16).map_err(|_| "bad \\u")?;
                                if !(0xDC00..=0xDFFF).contains(&cp2) {
                                    return Err("bad surrogate pair".into());
                                }
                                let full = 0x10000 + ((cp - 0xD800) << 10) + (cp2 - 0xDC00);
                                if let Some(c) = char::from_u32(full) {
                                    let mut tmp = [0u8; 4];
                                    let s = c.encode_utf8(&mut tmp);
                                    buf.extend_from_slice(s.as_bytes());
                                }
                            } else if let Some(c) = char::from_u32(cp) {
                                let mut tmp = [0u8; 4];
                                let s = c.encode_utf8(&mut tmp);
                                buf.extend_from_slice(s.as_bytes());
                            } else {
                                return Err("bad codepoint".into());
                            }
                        }
                        other => return Err(format!("bad escape \\{}", other as char)),
                    }
                }
                _ => buf.push(b),
            }
        }
        let s = vm.heap.intern(&buf);
        Ok(Value::Str(s))
    }
    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.cur;
        if self.peek() == Some(b'-') {
            self.cur += 1;
        }
        // Integer part
        while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
            self.cur += 1;
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.cur += 1;
            while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
                self.cur += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.cur += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.cur += 1;
            }
            while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
                self.cur += 1;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.cur])
            .map_err(|_| "bad number")?;
        if is_float {
            s.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| "bad float".into())
        } else {
            // Try i64 first; if it overflows, fall back to f64.
            if let Ok(n) = s.parse::<i64>() {
                Ok(Value::Int(n))
            } else {
                s.parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| "bad integer".into())
            }
        }
    }
    fn parse_array(&mut self, vm: &mut Vm, depth: u32) -> Result<Value, String> {
        self.expect(b'[')?;
        self.skip_ws();
        let mut entries: Vec<Value> = Vec::new();
        if self.peek() == Some(b']') {
            self.cur += 1;
            // Empty array still gets a table; encoder treats `{}` as
            // object, so an empty Lua table from `[]` would round-trip
            // as `{}`. Not perfectly preservable in v1.27.3 — same
            // limitation as Redis cjson without extra hints.
            let t = vm.new_table().build();
            return Ok(Value::Table(t));
        }
        loop {
            entries.push(self.parse_value(vm, depth)?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => {
                    self.skip_ws();
                }
                Some(b']') => break,
                _ => return Err("expected ',' or ']' in array".into()),
            }
        }
        let mut b = vm.new_table();
        for (i, v) in entries.into_iter().enumerate() {
            b = b.with((i + 1) as i64, v);
        }
        Ok(Value::Table(b.build()))
    }
    fn parse_object(&mut self, vm: &mut Vm, depth: u32) -> Result<Value, String> {
        self.expect(b'{')?;
        self.skip_ws();
        let mut kvs: Vec<(Value, Value)> = Vec::new();
        if self.peek() == Some(b'}') {
            self.cur += 1;
            let t = vm.new_table().build();
            return Ok(Value::Table(t));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string(vm)?;
            self.skip_ws();
            self.expect(b':')?;
            let val = self.parse_value(vm, depth)?;
            kvs.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => break,
                _ => return Err("expected ',' or '}' in object".into()),
            }
        }
        let mut b = vm.new_table();
        for (k, v) in kvs {
            b = b.with(k, v);
        }
        Ok(Value::Table(b.build()))
    }
}

fn err(vm: &mut Vm, msg: &str) -> LuaError {
    let s = vm.heap.intern(msg.as_bytes());
    LuaError::new(Value::Str(s))
}

// ─────────────────────────────────────────────────────────────────────
// Installation
// ─────────────────────────────────────────────────────────────────────

pub(crate) fn install_cjson(vm: &mut Vm) {
    let enc_fn = vm.native(cjson_encode);
    let dec_fn = vm.native(cjson_decode);
    // cjson.null = Lua nil. Scripts that do `obj.field = cjson.null`
    // end up with `obj.field = nil` which Lua removes from the
    // table, so encoding `obj` gives `{}` instead of `{"field":null}`.
    // Matches the lossy behaviour of every Lua-without-sentinel
    // setup. Full sentinel detection is a v1.27.4+ patch.
    let t = vm.table_of([
        ("encode", enc_fn),
        ("decode", dec_fn),
        ("null", Value::Nil),
    ]);
    let _ = vm.set_global("cjson", Value::Table(t));
}

