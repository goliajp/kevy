//! Wire-level helpers for [`crate::subscribe::Subscriber`] — extracted
//! to keep `subscribe.rs` under the 500-LOC house rule.
//!
//! Pure stateless functions: framing a command onto a `TcpStream`,
//! pulling the next pub/sub frame, and shaping a [`Reply`] (RESP2
//! `*N` array or RESP3 `>N` push) back into the user-facing
//! [`PubsubEvent`] variant. IO failures surface as [`KevyError::Io`];
//! protocol-shape mismatches as [`KevyError::Protocol`]; a server-side
//! close as [`KevyError::Closed`].

use crate::{KevyError, KevyResult};
use std::io::{Read, Write};
use std::net::TcpStream;

use kevy_embedded::PubsubFrame;
use kevy_resp::{Reply, encode_command};
use kevy_resp_client::{ReplyReadBuf, classify_pubsub};

use crate::subscribe::PubsubEvent;

/// Send `verb args...` to the open server connection as one RESP `*N`
/// frame. No buffering — the caller is expected to follow up with
/// `recv_remote` (server replies with a `subscribe`/`psubscribe`
/// confirmation frame per channel/pattern).
pub(crate) fn send_to(
    stream: &mut TcpStream,
    verb: &[u8],
    args: &[&[u8]],
) -> KevyResult<()> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(verb.to_vec());
    argv.extend(args.iter().map(|a| a.to_vec()));
    let mut frame = Vec::new();
    encode_command(&mut frame, &argv);
    Ok(stream.write_all(&frame)?)
}

/// Pull the next pub/sub frame from `stream`, parsing into a
/// [`PubsubEvent`]. Loops over read+parse until one complete reply is
/// in `buf`; a malformed frame returns [`KevyError::Protocol`], a
/// 0-length read returns [`KevyError::Closed`]. Trailing partial
/// frames stay in `buf` for the next call.
pub(crate) fn recv_remote(
    stream: &mut TcpStream,
    buf: &mut ReplyReadBuf,
) -> KevyResult<PubsubEvent> {
    let mut chunk = [0u8; 8192];
    loop {
        match buf.parse_next() {
            // The frame classifier is canonical in `kevy_resp_client`,
            // shared with the async client — no per-crate copy. A
            // classify failure is a protocol-shape error (bad frame /
            // unknown kind), so its `io::Error` message surfaces as
            // `KevyError::Protocol`, matching the malformed-parse arm
            // below and the pre-refactor local classifier's contract.
            Ok(Some(reply)) => {
                return classify_pubsub(reply).map_err(|e| KevyError::Protocol(e.to_string()));
            }
            Ok(None) => {}
            Err(_) => {
                return Err(KevyError::Protocol("malformed reply".into()));
            }
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(KevyError::Closed);
        }
        buf.extend(&chunk[..n]);
    }
}

/// Convert the embedded-mode [`PubsubFrame`] (delivered through the
/// per-conn `Subscription` channel) into the public [`PubsubEvent`].
/// Embedded counts are u64; the public type uses i64 to match the
/// remote `recv` path, so each arm widens with `as i64`.
pub(crate) fn frame_to_event(frame: PubsubFrame) -> PubsubEvent {
    match frame {
        PubsubFrame::Subscribe { channel, count } => PubsubEvent::Subscribe {
            channel,
            count: count as i64,
        },
        PubsubFrame::Psubscribe { pattern, count } => PubsubEvent::Psubscribe {
            pattern,
            count: count as i64,
        },
        PubsubFrame::Unsubscribe { channel, count } => PubsubEvent::Unsubscribe {
            channel,
            count: count as i64,
        },
        PubsubFrame::Punsubscribe { pattern, count } => PubsubEvent::Punsubscribe {
            pattern,
            count: count as i64,
        },
        PubsubFrame::Message { channel, payload } => PubsubEvent::Message { channel, payload },
        PubsubFrame::Pmessage {
            pattern,
            channel,
            payload,
        } => PubsubEvent::Pmessage {
            pattern,
            channel,
            payload,
        },
    }
}

pub(crate) fn shape(r: &Reply) -> &'static str {
    match r {
        Reply::Simple(_) => "simple-string",
        Reply::Error(_) => "error",
        Reply::Int(_) => "integer",
        Reply::Bulk(_) => "bulk-string",
        Reply::Nil | Reply::Null => "nil",
        Reply::Array(_) => "array",
        Reply::Map(_) => "map",
        Reply::Set(_) => "set",
        Reply::Double(_) => "double",
        Reply::Boolean(_) => "boolean",
        Reply::Verbatim { .. } => "verbatim-string",
        Reply::BigNumber(_) => "big-number",
        Reply::Push(_) => "push",
        Reply::BlobError(_) => "blob-error",
    }
}

pub(crate) fn invalid(msg: impl Into<String>) -> KevyError {
    KevyError::Protocol(msg.into())
}
