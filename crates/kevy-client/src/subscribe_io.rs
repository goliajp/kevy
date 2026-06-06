//! Wire-level helpers for [`crate::subscribe::Subscriber`] — extracted
//! to keep `subscribe.rs` under the 500-LOC house rule.
//!
//! Pure stateless functions: framing a command onto a `TcpStream`,
//! pulling the next pub/sub frame, and shaping a [`Reply`] (RESP2
//! `*N` array or RESP3 `>N` push) back into the user-facing
//! [`PubsubEvent`] variant. All io errors and protocol-shape mismatches
//! surface as `io::Error` with an `InvalidData` / `UnexpectedEof` kind
//! so callers can pattern-match without a custom error enum.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use kevy_embedded::PubsubFrame;
use kevy_resp::{Reply, encode_command, parse_reply};

use crate::subscribe::PubsubEvent;

/// Send `verb args...` to the open server connection as one RESP `*N`
/// frame. No buffering — the caller is expected to follow up with
/// `recv_remote` (server replies with a `subscribe`/`psubscribe`
/// confirmation frame per channel/pattern).
pub(crate) fn send_to(
    stream: &mut TcpStream,
    verb: &[u8],
    args: &[&[u8]],
) -> io::Result<()> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(verb.to_vec());
    argv.extend(args.iter().map(|a| a.to_vec()));
    let mut frame = Vec::new();
    encode_command(&mut frame, &argv);
    stream.write_all(&frame)
}

/// Pull the next pub/sub frame from `stream`, parsing into a
/// [`PubsubEvent`]. Loops over read+parse until one complete reply is
/// in `buf`; a malformed frame returns `InvalidData`, a 0-length read
/// returns `UnexpectedEof`. Trailing partial frames stay in `buf` for
/// the next call.
pub(crate) fn recv_remote(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> io::Result<PubsubEvent> {
    let mut chunk = [0u8; 8192];
    loop {
        match parse_reply(buf) {
            Ok(Some((reply, used))) => {
                buf.drain(..used);
                return classify(reply);
            }
            Ok(None) => {}
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed reply",
                ));
            }
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
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

/// Shape a raw RESP reply into the matching [`PubsubEvent`] variant.
/// RESP2 pub/sub frames arrive as `*N` arrays; RESP3 servers wrap the
/// same shape in a `>N` push frame so the client can demux out-of-band
/// deliveries from regular command replies. Both forms are accepted.
pub(crate) fn classify(reply: Reply) -> io::Result<PubsubEvent> {
    let items = match reply {
        Reply::Array(v) | Reply::Push(v) => v,
        other => return Err(invalid(format!("expected array frame, got {}", shape(&other)))),
    };
    let kind = match items.first() {
        Some(Reply::Bulk(b)) => b.clone(),
        _ => return Err(invalid("pubsub frame missing kind field")),
    };
    match kind.as_slice() {
        b"subscribe" => {
            let [_, ch, n] = into_array3(items)?;
            Ok(PubsubEvent::Subscribe {
                channel: take_bulk(ch, "channel")?,
                count: take_int(n, "count")?,
            })
        }
        b"psubscribe" => {
            let [_, p, n] = into_array3(items)?;
            Ok(PubsubEvent::Psubscribe {
                pattern: take_bulk(p, "pattern")?,
                count: take_int(n, "count")?,
            })
        }
        b"unsubscribe" => {
            let [_, ch, n] = into_array3(items)?;
            Ok(PubsubEvent::Unsubscribe {
                channel: take_bulk_or_nil(ch, "channel")?,
                count: take_int(n, "count")?,
            })
        }
        b"punsubscribe" => {
            let [_, p, n] = into_array3(items)?;
            Ok(PubsubEvent::Punsubscribe {
                pattern: take_bulk_or_nil(p, "pattern")?,
                count: take_int(n, "count")?,
            })
        }
        b"message" => {
            let [_, ch, payload] = into_array3(items)?;
            Ok(PubsubEvent::Message {
                channel: take_bulk(ch, "channel")?,
                payload: take_bulk(payload, "payload")?,
            })
        }
        b"pmessage" => {
            let [_, pat, ch, payload] = into_array4(items)?;
            Ok(PubsubEvent::Pmessage {
                pattern: take_bulk(pat, "pattern")?,
                channel: take_bulk(ch, "channel")?,
                payload: take_bulk(payload, "payload")?,
            })
        }
        other => Err(invalid(format!(
            "unknown pubsub kind '{}'",
            String::from_utf8_lossy(other)
        ))),
    }
}

fn into_array3(items: Vec<Reply>) -> io::Result<[Reply; 3]> {
    items
        .try_into()
        .map_err(|v: Vec<Reply>| invalid(format!("expected 3-element pubsub frame, got {}", v.len())))
}

fn into_array4(items: Vec<Reply>) -> io::Result<[Reply; 4]> {
    items
        .try_into()
        .map_err(|v: Vec<Reply>| invalid(format!("expected 4-element pubsub frame, got {}", v.len())))
}

fn take_bulk(r: Reply, field: &str) -> io::Result<Vec<u8>> {
    match r {
        Reply::Bulk(b) => Ok(b),
        other => Err(invalid(format!(
            "expected bulk for {field}, got {}",
            shape(&other)
        ))),
    }
}

fn take_bulk_or_nil(r: Reply, field: &str) -> io::Result<Option<Vec<u8>>> {
    match r {
        Reply::Bulk(b) => Ok(Some(b)),
        Reply::Nil => Ok(None),
        other => Err(invalid(format!(
            "expected bulk/nil for {field}, got {}",
            shape(&other)
        ))),
    }
}

fn take_int(r: Reply, field: &str) -> io::Result<i64> {
    match r {
        Reply::Int(n) => Ok(n),
        other => Err(invalid(format!(
            "expected integer for {field}, got {}",
            shape(&other)
        ))),
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

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}
