//! kevy-resp — a zero-dependency [RESP] (REdis Serialization Protocol) codec.
//!
//! It covers what a client sends to drive commands — the RESP2 multi-bulk
//! request (`*N\r\n$len\r\n…`) and the inline form (a bare `PING\r\n` typed over
//! a raw connection) — plus the reply primitives a server writes back. Parsing
//! is incremental and allocation-light: [`parse_command`] returns `Ok(None)`
//! when more bytes are needed, so it composes with a streaming read loop.
//!
//! Pure Rust, no dependencies. Part of the [kevy] key–value server.
//!
//! [RESP]: https://redis.io/docs/latest/develop/reference/protocol-spec/
//! [kevy]: https://crates.io/crates/kevy
//!
//! # Example
//!
//! ```
//! use kevy_resp::{encode_bulk, encode_simple_string, parse_command};
//!
//! // Parse one command from a request buffer.
//! let (cmd, consumed) = parse_command(b"*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n")
//!     .unwrap() // not a protocol error
//!     .unwrap(); // a complete frame was present
//! assert_eq!(cmd, vec![b"ECHO".to_vec(), b"hi".to_vec()]);
//! assert_eq!(consumed, 22);
//!
//! // A partial frame asks for more bytes rather than erroring.
//! assert_eq!(parse_command(b"*1\r\n$4\r\nPI").unwrap(), None);
//!
//! // Encode replies into a caller-owned buffer.
//! let mut out = Vec::new();
//! encode_simple_string(&mut out, "PONG");
//! encode_bulk(&mut out, b"hi");
//! assert_eq!(out, b"+PONG\r\n$2\r\nhi\r\n");
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// A parsed command's argument vector.
///
/// Stored in **two allocations** — all argument bytes concatenated in `buf`,
/// with `ends[i]` the end offset of argument `i` — instead of the `N+1` a
/// `Vec<Vec<u8>>` needs (one outer `Vec` plus one per argument). Parsing a SET
/// drops from 4 allocations to 2. It is `Send` (two `Vec`s), so the
/// thread-per-core runtime still forwards it across cores by value.
///
/// Index/`get`/`first`/`iter` return `&[u8]` argument slices. It compares equal
/// to a `Vec<Vec<u8>>` of the same arguments, so call sites and tests read
/// naturally.
#[derive(Clone, Default, Debug, Eq)]
pub struct Argv {
    buf: Vec<u8>,
    ends: Vec<u32>,
}

impl Argv {
    /// An empty argv, pre-sizing for `argc` args totalling `bytes` bytes.
    pub fn with_capacity(argc: usize, bytes: usize) -> Self {
        Argv {
            buf: Vec::with_capacity(bytes),
            ends: Vec::with_capacity(argc),
        }
    }

    /// Drop all args while keeping the buf + ends capacity. Used by the
    /// reactor's per-command scratch `Argv`: `parse_command_into` clears
    /// then refills, so the hot path's malloc rate drops to ~0.
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.ends.clear();
    }

    /// Reserve room for `argc` args totalling `bytes` bytes on top of what is
    /// already there (no shrink).
    #[inline]
    pub fn reserve_for(&mut self, argc: usize, bytes: usize) {
        self.buf.reserve(bytes);
        self.ends.reserve(argc);
    }

    /// Append one argument.
    pub fn push(&mut self, arg: &[u8]) {
        self.buf.extend_from_slice(arg);
        self.ends.push(self.buf.len() as u32);
    }

    /// Number of arguments.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Argument `i` as a byte slice, or `None` if out of range.
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let end = *self.ends.get(i)? as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        Some(&self.buf[start..end])
    }

    /// The first argument (the command name), or `None` if empty.
    pub fn first(&self) -> Option<&[u8]> {
        self.get(0)
    }

    /// Iterate the arguments as byte slices.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(move |i| self.get(i).expect("in range"))
    }
}

impl core::ops::Index<usize> for Argv {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        self.get(i).expect("argv index out of bounds")
    }
}

/// Compare to a `Vec<Vec<u8>>` of the same arguments (keeps call sites + tests
/// that build the expected value as a vec-of-vecs readable).
impl PartialEq<Vec<Vec<u8>>> for Argv {
    fn eq(&self, other: &Vec<Vec<u8>>) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b.as_slice())
    }
}

impl PartialEq for Argv {
    fn eq(&self, other: &Argv) -> bool {
        self.buf == other.buf && self.ends == other.ends
    }
}

/// Build from a vec-of-vecs (test/embedding convenience; the wire path uses
/// [`parse_command`], which builds an [`Argv`] directly without the intermediate
/// allocations).
impl From<Vec<Vec<u8>>> for Argv {
    fn from(v: Vec<Vec<u8>>) -> Self {
        let mut a = Argv::with_capacity(v.len(), v.iter().map(Vec::len).sum());
        for arg in &v {
            a.push(arg);
        }
        a
    }
}

/// A parsed command: `argv`, where `argv[0]` is the command name.
pub type Command = Argv;

/// Why a buffer could not (yet) be parsed into a command.
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// A malformed frame that can never become valid (e.g. bad length prefix).
    Malformed(&'static str),
}

/// Attempt to parse one command from the front of `buf`.
///
/// - `Ok(Some((cmd, consumed)))` — a full command; `consumed` bytes may be dropped.
/// - `Ok(None)` — need more bytes; call again after reading more.
/// - `Err(_)` — the stream is corrupt; the caller should reply with an error
///   and close the connection.
///
/// This is the convenience form that allocates a fresh `Argv` per call. The
/// reactor's hot path uses [`parse_command_into`] with a reused scratch
/// `Argv` to keep per-cmd malloc rate at 0.
pub fn parse_command(buf: &[u8]) -> Result<Option<(Command, usize)>, ProtocolError> {
    let mut argv = Argv::default();
    match parse_command_into(buf, &mut argv)? {
        Some(consumed) => Ok(Some((argv, consumed))),
        None => Ok(None),
    }
}

/// Same as [`parse_command`], but writes into a caller-provided scratch
/// `Argv` instead of allocating a new one each call. The reactor stores one
/// `Argv` per shard and reuses it for every cmd on the local hot path; the
/// internal `Vec<u8>` + `Vec<u32>` capacities amortise to zero allocations
/// per command after the first few cmds warm them.
///
/// `dst` is cleared at the start of every call; on `Ok(None)` and `Err`, `dst`
/// is left empty (so the caller doesn't see partial state).
pub fn parse_command_into(
    buf: &[u8],
    dst: &mut Argv,
) -> Result<Option<usize>, ProtocolError> {
    dst.clear();
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'*' {
        parse_multibulk_into(buf, dst)
    } else {
        parse_inline_into(buf, dst)
    }
}

fn parse_inline_into(buf: &[u8], dst: &mut Argv) -> Result<Option<usize>, ProtocolError> {
    let Some(eol) = find_crlf(buf, 0) else {
        return Ok(None);
    };
    let line = &buf[..eol];
    for tok in line
        .split(|b| b.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
    {
        dst.push(tok);
    }
    Ok(Some(eol + 2))
}

fn parse_multibulk_into(buf: &[u8], dst: &mut Argv) -> Result<Option<usize>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count =
        parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad multibulk count"))?;
    if count < 0 {
        // Null array → empty argv (already cleared).
        return Ok(Some(hdr_end + 2));
    }
    let count = count as usize;

    // Pass 1: validate the whole frame is present and sum total argument bytes.
    let mut pos = hdr_end + 2;
    let mut total = 0usize;
    for _ in 0..count {
        if pos >= buf.len() {
            return Ok(None);
        }
        if buf[pos] != b'$' {
            return Err(ProtocolError::Malformed("expected bulk string"));
        }
        let Some(len_end) = find_crlf(buf, pos + 1) else {
            return Ok(None);
        };
        let len =
            parse_int(&buf[pos + 1..len_end]).ok_or(ProtocolError::Malformed("bad bulk length"))?;
        if len < 0 {
            return Err(ProtocolError::Malformed("negative bulk length in request"));
        }
        let len = len as usize;
        let data_end = len_end + 2 + len;
        if buf.len() < data_end + 2 {
            return Ok(None);
        }
        if &buf[data_end..data_end + 2] != b"\r\n" {
            return Err(ProtocolError::Malformed("bulk string not CRLF-terminated"));
        }
        total += len;
        pos = data_end + 2;
    }

    // Pass 2: copy into dst (capacity already reserved if amortised; `reserve`
    // is a no-op when current capacity suffices).
    dst.reserve_for(count, total);
    let mut p = hdr_end + 2;
    for _ in 0..count {
        let len_end = find_crlf(buf, p + 1).expect("validated in pass 1");
        let len = parse_int(&buf[p + 1..len_end]).expect("validated in pass 1") as usize;
        let data_start = len_end + 2;
        dst.push(&buf[data_start..data_start + len]);
        p = data_start + len + 2;
    }
    Ok(Some(pos))
}

/// Find the index of `\r\n` at or after `start`, returning the index of `\r`.
///
/// SWAR-accelerated: scans 8 bytes at a time using the classic "has-zero-byte"
/// bit trick (XOR each byte with `\r`, then `(x - 0x01..) & !x & 0x80..`
/// isolates bytes that were zero). On a CR hit we confirm the next byte is
/// `\n` and return; otherwise we resume from `pos + 1` so a stray `\r` doesn't
/// terminate the scan. Safe Rust only — keeps `kevy-resp`'s
/// `forbid(unsafe_code)` charter line.
fn find_crlf(buf: &[u8], start: usize) -> Option<usize> {
    const CR_BCAST: u64 = 0x0D0D0D0D_0D0D0D0Du64;
    const ONES: u64 = 0x01010101_01010101u64;
    const HIGH: u64 = 0x80808080_80808080u64;

    let n = buf.len();
    let mut i = start;
    // Need at least 2 bytes (CR + LF) to find a CRLF.
    if i + 1 >= n {
        return None;
    }
    // SWAR loop: read 8 bytes, find any byte == 0x0D, then check the next
    // byte. We require the WHOLE 8-byte window to be within `buf` AND the
    // byte just past it to also exist (so a CR at position 7 of the window
    // can be confirmed by reading position 8). That's `i + 9 <= n`, i.e.
    // `i + 8 < n` (strict, since we may need [pos+1] which is at most i+8
    // when pos == i+7).
    while i + 8 < n {
        let word = u64::from_le_bytes(buf[i..i + 8].try_into().expect("8 bytes"));
        let x = word ^ CR_BCAST;
        let zeroed = x.wrapping_sub(ONES) & !x & HIGH;
        if zeroed != 0 {
            // The low set bit's byte index = first CR in this 8-byte window.
            let bit_idx = zeroed.trailing_zeros();
            let pos = i + (bit_idx / 8) as usize;
            // pos < i + 8 ≤ n - 1, so pos + 1 < n is valid to read.
            if buf[pos + 1] == b'\n' {
                return Some(pos);
            }
            // Lone CR — resume scanning from the byte after it.
            i = pos + 1;
            continue;
        }
        i += 8;
    }
    // Tail: scalar over the last < 8 bytes (or what's left after a partial
    // resume above).
    while i + 1 < n {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a base-10 signed integer from ASCII bytes (no surrounding whitespace).
fn parse_int(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let (neg, digits) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if neg { -acc } else { acc })
}

// ---- Reply encoders (append to a caller-owned buffer) ----------------------

/// `+<s>\r\n`
pub fn encode_simple_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'+');
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// `-<s>\r\n`
pub fn encode_error(out: &mut Vec<u8>, s: &str) {
    out.push(b'-');
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// `:<n>\r\n`
pub fn encode_integer(out: &mut Vec<u8>, n: i64) {
    out.push(b':');
    push_int(out, n);
    out.extend_from_slice(b"\r\n");
}

/// `$<len>\r\n<data>\r\n`
pub fn encode_bulk(out: &mut Vec<u8>, data: &[u8]) {
    // Reserve the whole frame up front so a fresh reply buffer (the common case:
    // dispatch hands each command an empty `Vec`) fills without repeated reallocs
    // as it grows — the bulk reply is the hot GET path. 16 covers '$', the length
    // digits, and both CRLFs.
    out.reserve(data.len() + 16);
    out.push(b'$');
    push_int(out, data.len() as i64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

/// `$-1\r\n` — the RESP2 null bulk string.
pub fn encode_null_bulk(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

/// `*<len>\r\n` — an array header; follow with `len` encoded elements.
pub fn encode_array_len(out: &mut Vec<u8>, len: i64) {
    out.push(b'*');
    push_int(out, len);
    out.extend_from_slice(b"\r\n");
}

/// Encode a command as a RESP multi-bulk request (client → server):
/// `*N\r\n$len\r\n<arg>\r\n…`. The inverse of [`parse_command`].
pub fn encode_command(out: &mut Vec<u8>, args: &[Vec<u8>]) {
    encode_array_len(out, args.len() as i64);
    for a in args {
        encode_bulk(out, a);
    }
}

/// Append the base-10 representation of `n` without allocating an intermediate
/// String. Handles `i64::MIN` correctly.
fn push_int(out: &mut Vec<u8>, n: i64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    // Work in the negative domain so i64::MIN doesn't overflow.
    let neg = n < 0;
    let mut v = n;
    while v != 0 {
        let digit = (v % 10).unsigned_abs() as u8;
        i -= 1;
        tmp[i] = b'0' + digit;
        v /= 10;
    }
    if neg {
        out.push(b'-');
    }
    out.extend_from_slice(&tmp[i..]);
}

// ---- Reply parsing (client side) -------------------------------------------

/// A parsed RESP reply (server → client) — the client-side counterpart of the
/// `encode_*` functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `+OK`
    Simple(Vec<u8>),
    /// `-ERR ...`
    Error(Vec<u8>),
    /// `:42`
    Int(i64),
    /// `$5\r\nhello\r\n`
    Bulk(Vec<u8>),
    /// `$-1` or `*-1`
    Nil,
    /// `*N ...`
    Array(Vec<Reply>),
}

/// Parse one RESP reply from the front of `buf`.
///
/// - `Ok(Some((reply, consumed)))` — a complete reply.
/// - `Ok(None)` — need more bytes.
/// - `Err(_)` — malformed.
pub fn parse_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(&tag) = buf.first() else {
        return Ok(None);
    };
    match tag {
        b'+' => Ok(reply_line(buf).map(|(b, used)| (Reply::Simple(b.to_vec()), used))),
        b'-' => Ok(reply_line(buf).map(|(b, used)| (Reply::Error(b.to_vec()), used))),
        b':' => match reply_line(buf) {
            None => Ok(None),
            Some((b, used)) => {
                let n = parse_int(b).ok_or(ProtocolError::Malformed("bad integer reply"))?;
                Ok(Some((Reply::Int(n), used)))
            }
        },
        b'$' => parse_bulk_reply(buf),
        b'*' => parse_array_reply(buf),
        _ => Err(ProtocolError::Malformed("unknown reply type")),
    }
}

/// The CRLF-terminated payload after the type byte, plus bytes consumed.
fn reply_line(buf: &[u8]) -> Option<(&[u8], usize)> {
    find_crlf(buf, 1).map(|eol| (&buf[1..eol], eol + 2))
}

fn parse_bulk_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let len = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad bulk length"))?;
    if len < 0 {
        return Ok(Some((Reply::Nil, hdr_end + 2)));
    }
    let data_start = hdr_end + 2;
    let data_end = data_start + len as usize;
    if buf.len() < data_end + 2 {
        return Ok(None);
    }
    Ok(Some((
        Reply::Bulk(buf[data_start..data_end].to_vec()),
        data_end + 2,
    )))
}

fn parse_array_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ProtocolError> {
    let Some(hdr_end) = find_crlf(buf, 1) else {
        return Ok(None);
    };
    let count = parse_int(&buf[1..hdr_end]).ok_or(ProtocolError::Malformed("bad array length"))?;
    if count < 0 {
        return Ok(Some((Reply::Nil, hdr_end + 2)));
    }
    let mut pos = hdr_end + 2;
    // Cap initial capacity by remaining buffer bytes — an attacker-controlled
    // `*999999999999\r\n` header would otherwise panic via `Vec::with_capacity`'s
    // capacity overflow. Each item costs ≥ 1 byte (a CRLF for Nil/Int/Simple),
    // so a real array of N items needs ≥ N bytes left. Push will grow the vec
    // amortized if the genuine count is higher but bytes are present. Found by
    // cargo-fuzz against crash-4c4ee6777903d009f93289eb428b3b371d027137 during
    // STONE-AUDIT Phase A #4 (2026-05-26).
    let cap = (count as usize).min(buf.len().saturating_sub(pos));
    let mut items = Vec::with_capacity(cap);
    for _ in 0..count {
        match parse_reply(&buf[pos..])? {
            None => return Ok(None),
            Some((r, used)) => {
                items.push(r);
                pos += used;
            }
        }
    }
    Ok(Some((Reply::Array(items), pos)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // SWAR find_crlf fuzz: planted CRLFs at every offset 0..40, lone-CR
    // distractors, no-CRLF inputs, near-end boundaries. The SWAR window is
    // 8 bytes, so transitions at offsets 0/7/8/15/16/… stress alignment.
    #[test]
    fn find_crlf_at_every_offset() {
        for off in 0..40 {
            let mut buf = vec![b'a'; 60];
            buf[off] = b'\r';
            buf[off + 1] = b'\n';
            assert_eq!(find_crlf(&buf, 0), Some(off), "off={off}");
        }
    }

    #[test]
    fn find_crlf_skips_lone_cr() {
        // Lone \r at the front, then a real CRLF later.
        let mut buf = vec![b'a'; 32];
        buf[3] = b'\r';
        buf[4] = b'b'; // not \n → skip
        buf[20] = b'\r';
        buf[21] = b'\n';
        assert_eq!(find_crlf(&buf, 0), Some(20));
    }

    #[test]
    fn find_crlf_none_when_absent() {
        let buf = vec![b'a'; 32];
        assert_eq!(find_crlf(&buf, 0), None);
        let buf = b"";
        assert_eq!(find_crlf(buf, 0), None);
        let buf = b"\r"; // only CR, no LF available
        assert_eq!(find_crlf(buf, 0), None);
    }

    #[test]
    fn find_crlf_at_buffer_end() {
        let buf = b"abcdefghij\r\n"; // CRLF at offset 10
        assert_eq!(find_crlf(buf, 0), Some(10));
        // Start past the CR.
        assert_eq!(find_crlf(buf, 11), None);
    }

    #[test]
    fn find_crlf_with_many_lone_crs() {
        // 7 lone CRs followed by a real CRLF. SWAR finds one CR per iter
        // but must keep going until it finds the real pair.
        let mut buf = Vec::new();
        for _ in 0..7 {
            buf.push(b'\r');
            buf.push(b'x'); // not \n
        }
        buf.extend_from_slice(b"\r\n");
        // Real CRLF starts at offset 14 (7 * 2).
        assert_eq!(find_crlf(&buf, 0), Some(14));
    }

    #[test]
    fn find_crlf_from_nonzero_start() {
        let buf = b"\r\n\r\n\r\n";
        // Starts at offset 0 → first CRLF.
        assert_eq!(find_crlf(buf, 0), Some(0));
        // Skip the first CRLF.
        assert_eq!(find_crlf(buf, 2), Some(2));
        assert_eq!(find_crlf(buf, 4), Some(4));
    }

    #[test]
    fn parse_multibulk_ping() {
        let (cmd, used) = parse_command(b"*1\r\n$4\r\nPING\r\n").unwrap().unwrap();
        assert_eq!(cmd, vec![b"PING".to_vec()]);
        assert_eq!(used, 14);
    }

    #[test]
    fn parse_multibulk_echo() {
        let frame = b"*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n";
        let (cmd, used) = parse_command(frame).unwrap().unwrap();
        assert_eq!(cmd, vec![b"ECHO".to_vec(), b"hello".to_vec()]);
        assert_eq!(used, frame.len());
    }

    #[test]
    fn parse_incomplete_returns_none() {
        assert_eq!(parse_command(b"*1\r\n$4\r\nPI").unwrap(), None);
        assert_eq!(parse_command(b"*2\r\n$4\r\nECHO\r\n").unwrap(), None);
        assert_eq!(parse_command(b"").unwrap(), None);
    }

    #[test]
    fn parse_inline_command() {
        let (cmd, used) = parse_command(b"PING\r\n").unwrap().unwrap();
        assert_eq!(cmd, vec![b"PING".to_vec()]);
        assert_eq!(used, 6);
        let (cmd, _) = parse_command(b"ECHO  hi there\r\n").unwrap().unwrap();
        assert_eq!(
            cmd,
            vec![b"ECHO".to_vec(), b"hi".to_vec(), b"there".to_vec()]
        );
    }

    #[test]
    fn parse_malformed_errors() {
        assert!(parse_command(b"*1\r\n+OK\r\n").is_err());
        assert!(parse_command(b"*x\r\n").is_err());
    }

    #[test]
    fn round_trip_command() {
        let mut buf = Vec::new();
        encode_command(&mut buf, &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
        let (cmd, used) = parse_command(&buf).unwrap().unwrap();
        assert_eq!(cmd, vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
        assert_eq!(used, buf.len());
    }

    #[test]
    fn parse_replies() {
        let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
        assert_eq!(r(b"+OK\r\n"), Reply::Simple(b"OK".to_vec()));
        assert_eq!(r(b"-ERR bad\r\n"), Reply::Error(b"ERR bad".to_vec()));
        assert_eq!(r(b":42\r\n"), Reply::Int(42));
        assert_eq!(r(b"$5\r\nhello\r\n"), Reply::Bulk(b"hello".to_vec()));
        assert_eq!(r(b"$-1\r\n"), Reply::Nil);
        assert_eq!(r(b"*-1\r\n"), Reply::Nil);

        let (arr, used) = parse_reply(b"*2\r\n:1\r\n$2\r\nhi\r\n").unwrap().unwrap();
        assert_eq!(
            arr,
            Reply::Array(vec![Reply::Int(1), Reply::Bulk(b"hi".to_vec())])
        );
        assert_eq!(used, 16);

        // Incomplete replies ask for more bytes.
        assert_eq!(parse_reply(b"$5\r\nhel").unwrap(), None);
        assert_eq!(parse_reply(b"*2\r\n:1\r\n").unwrap(), None);
        assert!(parse_reply(b"!nope\r\n").is_err());
    }

    #[test]
    fn encoders_match_resp2() {
        let mut out = Vec::new();
        encode_simple_string(&mut out, "PONG");
        assert_eq!(out, b"+PONG\r\n");

        out.clear();
        encode_bulk(&mut out, b"hello");
        assert_eq!(out, b"$5\r\nhello\r\n");

        out.clear();
        encode_error(&mut out, "ERR nope");
        assert_eq!(out, b"-ERR nope\r\n");

        out.clear();
        encode_integer(&mut out, -1234);
        assert_eq!(out, b":-1234\r\n");

        out.clear();
        encode_integer(&mut out, i64::MIN);
        assert_eq!(out, b":-9223372036854775808\r\n");

        out.clear();
        encode_null_bulk(&mut out);
        assert_eq!(out, b"$-1\r\n");
    }
}
