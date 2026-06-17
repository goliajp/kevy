//! Request-side parser: turns a byte stream from a client into an [`Argv`].
//!
//! Handles the two RESP2 request forms — `*N\r\n$L\r\n…` multi-bulk (the
//! normal client encoding) and the inline form (whitespace-separated, a
//! convenience for raw-typed PING / DEBUG / etc). Parsing is incremental:
//! returning `Ok(None)` asks the caller to read more bytes and retry.

use crate::argv::{Argv, Command};
use crate::error::ProtocolError;

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
pub fn parse_command_into(buf: &[u8], dst: &mut Argv) -> Result<Option<usize>, ProtocolError> {
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

// Signature mirrors `parse_multibulk_into` so `parse_command_into` can dispatch
// on the leading byte without converting between Result and Option shapes —
// the inline path can't actually fail, but the Result wrap is a price we pay
// for arm symmetry, not a hidden error path.
#[allow(clippy::unnecessary_wraps)]
fn parse_inline_into(buf: &[u8], dst: &mut Argv) -> Result<Option<usize>, ProtocolError> {
    let Some(eol) = find_crlf(buf, 0) else {
        return Ok(None);
    };
    let line = &buf[..eol];
    for tok in line
        .split(u8::is_ascii_whitespace)
        .filter(|s| !s.is_empty())
    {
        dst.push(tok);
    }
    Ok(Some(eol + 2))
}

/// Validate the multi-bulk frame is fully present and report `(end_pos,
/// total_arg_bytes)` if so. `start_pos` is the offset of the first `$`
/// after the `*N\r\n` header. `Ok(None)` = need more bytes; `Err` = malformed.
pub(crate) fn validate_multibulk_frame(
    buf: &[u8],
    start_pos: usize,
    count: usize,
) -> Result<Option<(usize, usize)>, ProtocolError> {
    let mut pos = start_pos;
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
        let len = parse_int(&buf[pos + 1..len_end])
            .ok_or(ProtocolError::Malformed("bad bulk length"))?;
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
    Ok(Some((pos, total)))
}

/// Copy `count` already-validated bulk args from `buf[start_pos..]` into `dst`.
/// Caller must have called [`validate_multibulk_frame`] first.
fn copy_multibulk_args(buf: &[u8], start_pos: usize, count: usize, dst: &mut Argv) {
    let mut p = start_pos;
    for _ in 0..count {
        let len_end = find_crlf(buf, p + 1).expect("validated in pass 1");
        let len = parse_int(&buf[p + 1..len_end]).expect("validated in pass 1") as usize;
        let data_start = len_end + 2;
        dst.push(&buf[data_start..data_start + len]);
        p = data_start + len + 2;
    }
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
    let start = hdr_end + 2;

    let Some((end_pos, total)) = validate_multibulk_frame(buf, start, count)? else {
        return Ok(None);
    };

    // `reserve` is a no-op when the scratch Argv has already amortised
    // enough capacity from earlier cmds.
    dst.reserve_for(count, total);
    copy_multibulk_args(buf, start, count, dst);
    Ok(Some(end_pos))
}

/// Parse a bulk-string length header `$<len>\r\n` whose `$` sits at
/// `buf[pos]` (the caller has already checked that byte). One fused pass:
/// the digits accumulate while the same loop walks to the terminating
/// CRLF — bulk headers are 2-21 bytes, so this short byte loop beats the
/// `find_crlf` + [`parse_int`] double scan the two-pass parser paid per
/// arg. Accepts the same shapes as `parse_int` (optional `+`/`-` sign,
/// checked i64 accumulation); a negative length is malformed in a
/// request, matching [`validate_multibulk_frame`].
///
/// Returns `(len, data_start)`; `Ok(None)` = need more bytes.
pub(crate) fn parse_bulk_len(
    buf: &[u8],
    pos: usize,
) -> Result<Option<(usize, usize)>, ProtocolError> {
    let mut q = pos + 1;
    let neg = match buf.get(q) {
        None => return Ok(None),
        Some(b'-') => {
            q += 1;
            true
        }
        Some(b'+') => {
            q += 1;
            false
        }
        _ => false,
    };
    let digits_start = q;
    let mut acc: i64 = 0;
    loop {
        match buf.get(q) {
            None => return Ok(None),
            Some(&b) if b.is_ascii_digit() => {
                acc = acc
                    .checked_mul(10)
                    .and_then(|a| a.checked_add(i64::from(b - b'0')))
                    .ok_or(ProtocolError::Malformed("bad bulk length"))?;
                q += 1;
            }
            Some(b'\r') => break,
            Some(_) => return Err(ProtocolError::Malformed("bad bulk length")),
        }
    }
    if q == digits_start {
        return Err(ProtocolError::Malformed("bad bulk length"));
    }
    match buf.get(q + 1) {
        None => return Ok(None),
        Some(b'\n') => {}
        Some(_) => return Err(ProtocolError::Malformed("bad bulk length")),
    }
    if neg {
        return Err(ProtocolError::Malformed("negative bulk length in request"));
    }
    Ok(Some((acc as usize, q + 2)))
}

/// Find the index of `\r\n` at or after `start`, returning the index of `\r`.
///
/// SWAR-accelerated: scans 8 bytes at a time using the classic "has-zero-byte"
/// bit trick (XOR each byte with `\r`, then `(x - 0x01..) & !x & 0x80..`
/// isolates bytes that were zero). On a CR hit we confirm the next byte is
/// `\n` and return; otherwise we resume from `pos + 1` so a stray `\r` doesn't
/// terminate the scan. Safe Rust only — keeps `kevy-resp`'s
/// `forbid(unsafe_code)` guarantee.
pub(crate) fn find_crlf(buf: &[u8], start: usize) -> Option<usize> {
    const CR_BCAST: u64 = 0x0D0D_0D0D_0D0D_0D0D_u64;
    const ONES: u64 = 0x0101_0101_0101_0101_u64;
    const HIGH: u64 = 0x8080_8080_8080_8080_u64;

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
pub(crate) fn parse_int(bytes: &[u8]) -> Option<i64> {
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
        acc = acc.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    Some(if neg { -acc } else { acc })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_command;

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

}
