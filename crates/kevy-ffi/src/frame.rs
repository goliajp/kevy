//! RESP frame encoding for the polled pub/sub drain.
//!
//! Split out of `lib.rs` (the house 500-LOC rule) but semantically one unit:
//! [`encode_frame`] renders a [`PubsubEvent`] byte-for-byte as the array the
//! server pushes on the wire, so every binding's RESP parser handles live
//! frames and command replies with the same code. The raw scalar lane
//! (`kevy_sub_next_raw`) bypasses all of this.

use kevy_embedded::PubsubEvent;

/// Encode a [`PubsubEvent`] exactly as the server pushes it on the wire.
pub(crate) fn encode_frame(f: &PubsubEvent) -> Vec<u8> {
    let mut out = Vec::new();
    match f {
        PubsubEvent::Message { channel, payload } => {
            arr(&mut out, &[b"message", channel, payload]);
        }
        PubsubEvent::Pmessage { pattern, channel, payload } => {
            arr(&mut out, &[b"pmessage", pattern, channel, payload]);
        }
        PubsubEvent::Subscribe { channel, count } => {
            ack(&mut out, b"subscribe", Some(channel), *count)
        }
        PubsubEvent::Psubscribe { pattern, count } => {
            ack(&mut out, b"psubscribe", Some(pattern), *count)
        }
        PubsubEvent::Unsubscribe { channel, count } => {
            ack(&mut out, b"unsubscribe", channel.as_deref(), *count)
        }
        PubsubEvent::Punsubscribe { pattern, count } => {
            ack(&mut out, b"punsubscribe", pattern.as_deref(), *count)
        }
        // an event kind newer than this encoder still reaches the binding
        // as a frame its RESP parser reads, rather than as an empty buffer
        _ => out.extend_from_slice(b"-ERR pubsub event kind unknown to this binding\r\n"),
    }
    out
}

fn arr(out: &mut Vec<u8>, items: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
    for it in items {
        bulk(out, it);
    }
}

fn ack(out: &mut Vec<u8>, kind: &[u8], name: Option<&[u8]>, count: i64) {
    out.extend_from_slice(b"*3\r\n");
    bulk(out, kind);
    match name {
        Some(n) => bulk(out, n),
        None => out.extend_from_slice(b"$-1\r\n"),
    }
    out.extend_from_slice(format!(":{count}\r\n").as_bytes());
}

fn bulk(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
    out.extend_from_slice(b);
    out.extend_from_slice(b"\r\n");
}
