//! Pub/sub delivery frames, in the subscriber's protocol: an array
//! under RESP2, a push frame under RESP3.

use kevy_resp::{RespVersion, encode_array_len, encode_bulk, encode_push_header};

/// Build a RESP pub/sub `message` delivery frame. V2 emits a 3-element
/// array (`*3\r\n…`); V3 emits a Push frame (`>3\r\n…`) — same body
/// bytes, prefix flips so the V3 client demuxes pub/sub from regular
/// replies. Per-conn proto, since one channel can have V2 + V3
/// subscribers mixed.
pub(crate) fn pubsub_message(channel: &[u8], msg: &[u8], proto: RespVersion) -> Vec<u8> {
    let mut out = Vec::new();
    match proto {
        RespVersion::V2 => encode_array_len(&mut out, 3),
        RespVersion::V3 => encode_push_header(&mut out, 3),
    }
    encode_bulk(&mut out, b"message");
    encode_bulk(&mut out, channel);
    encode_bulk(&mut out, msg);
    out
}

/// Pubsub `message` frame WITHOUT the body payload —
/// emits everything up to (but not including) the message bytes and the
/// trailing CRLF. The caller writes `<header><body><\r\n>` where the
/// `<body>` is splice-inserted via `Conn::output_arcs` (zero memcpy of
/// the body per subscriber). Same wire shape as [`pubsub_message`] once
/// re-assembled by `flush_conn` / the io_uring writev path.
///
/// Layout emitted: `*3\r\n$7\r\nmessage\r\n$<chlen>\r\n<channel>\r\n$<msglen>\r\n`
/// (V2), or `>3\r\n...` (V3). Caller then records `pos = output.len()`,
/// pushes an `Arc<[u8]>` of the body, then extends `output` with `\r\n`.
pub(crate) fn pubsub_message_header(
    out: &mut Vec<u8>,
    channel: &[u8],
    msg_len: usize,
    proto: RespVersion,
) {
    match proto {
        RespVersion::V2 => encode_array_len(out, 3),
        RespVersion::V3 => encode_push_header(out, 3),
    }
    encode_bulk(out, b"message");
    encode_bulk(out, channel);
    // `$<msg_len>\r\n` — the body's length prefix; body bytes follow
    // via the arc splice, then a trailing CRLF (caller's responsibility).
    out.push(b'$');
    let mut digits = [0u8; 20];
    let mut n = msg_len;
    let mut i = digits.len();
    if n == 0 {
        i -= 1;
        digits[i] = b'0';
    } else {
        while n > 0 {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    out.extend_from_slice(&digits[i..]);
    out.extend_from_slice(b"\r\n");
}

/// Build a RESP pub/sub `pmessage` delivery frame (PSUBSCRIBE matches).
/// V2 `*4\r\n…` Array vs V3 `>4\r\n…` Push frame — same body, prefix
/// flips.
pub(crate) fn pubsub_pmessage(
    pattern: &[u8],
    channel: &[u8],
    msg: &[u8],
    proto: RespVersion,
) -> Vec<u8> {
    let mut out = Vec::new();
    match proto {
        RespVersion::V2 => encode_array_len(&mut out, 4),
        RespVersion::V3 => encode_push_header(&mut out, 4),
    }
    encode_bulk(&mut out, b"pmessage");
    encode_bulk(&mut out, pattern);
    encode_bulk(&mut out, channel);
    encode_bulk(&mut out, msg);
    out
}
