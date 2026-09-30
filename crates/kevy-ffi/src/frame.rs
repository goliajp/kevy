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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsubscribe_ack_names_its_channel_or_sends_a_nil_bulk() {
        let named = PubsubEvent::Unsubscribe { channel: Some(b"news".to_vec()), count: 2 };
        assert_eq!(encode_frame(&named), b"*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:2\r\n");
        let all = PubsubEvent::Unsubscribe { channel: None, count: 0 };
        assert_eq!(encode_frame(&all), b"*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n");
    }

    #[test]
    fn a_punsubscribe_ack_names_its_pattern_or_sends_a_nil_bulk() {
        let named = PubsubEvent::Punsubscribe { pattern: Some(b"n.*".to_vec()), count: 1 };
        assert_eq!(encode_frame(&named), b"*3\r\n$12\r\npunsubscribe\r\n$3\r\nn.*\r\n:1\r\n");
        let all = PubsubEvent::Punsubscribe { pattern: None, count: 0 };
        assert_eq!(encode_frame(&all), b"*3\r\n$12\r\npunsubscribe\r\n$-1\r\n:0\r\n");
    }
}
