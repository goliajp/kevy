//! The pubsub frame vocabulary shared by every kevy client crate:
//! [`PubsubEvent`] (one received frame, acks and deliveries alike)
//! and [`classify_pubsub`] (RESP reply → event, handling both RESP2
//! `*N` arrays and RESP3 `>N` push frames).

use std::io;

use kevy_resp::Reply;

/// One pubsub frame received from the bus or the wire.
///
/// `Unsubscribe` / `Punsubscribe`'s `channel` / `pattern` is `None`
/// when the server acknowledges "unsubscribed from everything" with a
/// nil bulk — matching the Redis wire shape.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubsubEvent {
    /// `SUBSCRIBE` ack per channel.
    Subscribe {
        /// Channel that was just subscribed.
        channel: Vec<u8>,
        /// Total channels + patterns subscribed.
        count: i64,
    },
    /// `PSUBSCRIBE` ack per pattern.
    Psubscribe {
        /// Pattern that was just subscribed.
        pattern: Vec<u8>,
        /// Total channels + patterns subscribed.
        count: i64,
    },
    /// `UNSUBSCRIBE` ack.
    Unsubscribe {
        /// Channel just unsubscribed (`None` for "all"/"none" nil bulk).
        channel: Option<Vec<u8>>,
        /// Total channels + patterns still subscribed.
        count: i64,
    },
    /// `PUNSUBSCRIBE` ack.
    Punsubscribe {
        /// Pattern just unsubscribed (`None` for "all"/"none" nil bulk).
        pattern: Option<Vec<u8>>,
        /// Total channels + patterns still subscribed.
        count: i64,
    },
    /// Plain `PUBLISH` delivery on a subscribed channel.
    Message {
        /// Channel the publish was made to.
        channel: Vec<u8>,
        /// Raw payload bytes.
        payload: Vec<u8>,
    },
    /// Pattern-match delivery.
    Pmessage {
        /// Pattern the channel matched.
        pattern: Vec<u8>,
        /// Channel the publish was made to.
        channel: Vec<u8>,
        /// Raw payload bytes.
        payload: Vec<u8>,
    },
}

/// Turn a RESP reply into a [`PubsubEvent`]. Handles both RESP2
/// (`*N\r\n…` arrays) and RESP3 (`>N\r\n…` push frames).
// LOC-WAIVER: data-driven pubsub-kind match table — one flat frame-destructure arm per kind.
pub fn classify_pubsub(reply: Reply) -> io::Result<PubsubEvent> {
    let items = match reply {
        Reply::Array(v) | Reply::Push(v) => v,
        Reply::Error(e) => return Err(io::Error::other(String::from_utf8_lossy(&e).into_owned())),
        other => {
            return Err(invalid(format!("pubsub: expected array/push, got {}", shape(&other))));
        }
    };

    let mut it = items.into_iter();
    let kind = take_bulk(it.next().ok_or_else(|| invalid("pubsub: empty frame"))?, "kind")?;

    match kind.as_slice() {
        b"subscribe" => {
            let channel = take_bulk(
                it.next().ok_or_else(|| invalid("subscribe: missing channel"))?,
                "channel",
            )?;
            let count =
                take_int(it.next().ok_or_else(|| invalid("subscribe: missing count"))?, "count")?;
            Ok(PubsubEvent::Subscribe { channel, count })
        }
        b"psubscribe" => {
            let pattern = take_bulk(
                it.next().ok_or_else(|| invalid("psubscribe: missing pattern"))?,
                "pattern",
            )?;
            let count =
                take_int(it.next().ok_or_else(|| invalid("psubscribe: missing count"))?, "count")?;
            Ok(PubsubEvent::Psubscribe { pattern, count })
        }
        b"unsubscribe" => {
            let channel = take_bulk_or_nil(
                it.next().ok_or_else(|| invalid("unsubscribe: missing channel"))?,
                "channel",
            )?;
            let count =
                take_int(it.next().ok_or_else(|| invalid("unsubscribe: missing count"))?, "count")?;
            Ok(PubsubEvent::Unsubscribe { channel, count })
        }
        b"punsubscribe" => {
            let pattern = take_bulk_or_nil(
                it.next().ok_or_else(|| invalid("punsubscribe: missing pattern"))?,
                "pattern",
            )?;
            let count = take_int(
                it.next().ok_or_else(|| invalid("punsubscribe: missing count"))?,
                "count",
            )?;
            Ok(PubsubEvent::Punsubscribe { pattern, count })
        }
        b"message" => {
            let channel = take_bulk(
                it.next().ok_or_else(|| invalid("message: missing channel"))?,
                "channel",
            )?;
            let payload = take_bulk(
                it.next().ok_or_else(|| invalid("message: missing payload"))?,
                "payload",
            )?;
            Ok(PubsubEvent::Message { channel, payload })
        }
        b"pmessage" => {
            let pattern = take_bulk(
                it.next().ok_or_else(|| invalid("pmessage: missing pattern"))?,
                "pattern",
            )?;
            let channel = take_bulk(
                it.next().ok_or_else(|| invalid("pmessage: missing channel"))?,
                "channel",
            )?;
            let payload = take_bulk(
                it.next().ok_or_else(|| invalid("pmessage: missing payload"))?,
                "payload",
            )?;
            Ok(PubsubEvent::Pmessage { pattern, channel, payload })
        }
        other => Err(invalid(format!("unknown pubsub kind: {}", String::from_utf8_lossy(other)))),
    }
}

fn take_bulk(r: Reply, field: &str) -> io::Result<Vec<u8>> {
    match r {
        Reply::Bulk(v) | Reply::Simple(v) => Ok(v),
        other => {
            Err(invalid(format!("pubsub field {field}: expected bulk, got {}", shape(&other))))
        }
    }
}

fn take_bulk_or_nil(r: Reply, field: &str) -> io::Result<Option<Vec<u8>>> {
    match r {
        Reply::Bulk(v) | Reply::Simple(v) => Ok(Some(v)),
        Reply::Nil | Reply::Null => Ok(None),
        other => {
            Err(invalid(format!("pubsub field {field}: expected bulk/nil, got {}", shape(&other))))
        }
    }
}

fn take_int(r: Reply, field: &str) -> io::Result<i64> {
    match r {
        Reply::Int(n) => Ok(n),
        other => Err(invalid(format!("pubsub field {field}: expected int, got {}", shape(&other)))),
    }
}

fn shape(r: &Reply) -> &'static str {
    match r {
        Reply::Simple(_) => "simple",
        Reply::Error(_) => "error",
        Reply::Int(_) => "int",
        Reply::Bulk(_) => "bulk",
        Reply::Nil | Reply::Null => "nil",
        Reply::Array(_) => "array",
        Reply::Map(_) => "map",
        Reply::Set(_) => "set",
        Reply::Double(_) => "double",
        Reply::Boolean(_) => "boolean",
        Reply::Verbatim { .. } => "verbatim",
        Reply::BigNumber(_) => "bignumber",
        Reply::Push(_) => "push",
        Reply::BlobError(_) => "bloberror",
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_subscribe_ack() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"subscribe".to_vec()),
            Reply::Bulk(b"chan".to_vec()),
            Reply::Int(1),
        ]);
        assert_eq!(
            classify_pubsub(r).unwrap(),
            PubsubEvent::Subscribe { channel: b"chan".to_vec(), count: 1 }
        );
    }

    #[test]
    fn classify_message_event() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"message".to_vec()),
            Reply::Bulk(b"news".to_vec()),
            Reply::Bulk(b"hello".to_vec()),
        ]);
        assert_eq!(
            classify_pubsub(r).unwrap(),
            PubsubEvent::Message { channel: b"news".to_vec(), payload: b"hello".to_vec() }
        );
    }

    #[test]
    fn classify_pmessage_event() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"pmessage".to_vec()),
            Reply::Bulk(b"news.*".to_vec()),
            Reply::Bulk(b"news.tech".to_vec()),
            Reply::Bulk(b"hi".to_vec()),
        ]);
        assert_eq!(
            classify_pubsub(r).unwrap(),
            PubsubEvent::Pmessage {
                pattern: b"news.*".to_vec(),
                channel: b"news.tech".to_vec(),
                payload: b"hi".to_vec(),
            }
        );
    }

    #[test]
    fn classify_unsubscribe_with_nil_channel() {
        let r = Reply::Array(vec![Reply::Bulk(b"unsubscribe".to_vec()), Reply::Nil, Reply::Int(0)]);
        assert_eq!(
            classify_pubsub(r).unwrap(),
            PubsubEvent::Unsubscribe { channel: None, count: 0 }
        );
    }

    #[test]
    fn classify_accepts_push_frame() {
        // RESP3 servers wrap the same shape in a `>N` push frame.
        let r = Reply::Push(vec![
            Reply::Bulk(b"message".to_vec()),
            Reply::Bulk(b"c".to_vec()),
            Reply::Bulk(b"p".to_vec()),
        ]);
        assert_eq!(
            classify_pubsub(r).unwrap(),
            PubsubEvent::Message { channel: b"c".to_vec(), payload: b"p".to_vec() }
        );
    }

    #[test]
    fn classify_accepts_simple_string_fields() {
        // `take_bulk` accepts `Simple` as well as `Bulk` — a server may
        // send the kind/channel as simple strings.
        let r = Reply::Array(vec![
            Reply::Simple(b"subscribe".to_vec()),
            Reply::Simple(b"chan".to_vec()),
            Reply::Int(2),
        ]);
        assert_eq!(
            classify_pubsub(r).unwrap(),
            PubsubEvent::Subscribe { channel: b"chan".to_vec(), count: 2 }
        );
    }

    #[test]
    fn classify_rejects_unknown_kind() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"bogus".to_vec()),
            Reply::Bulk(b"x".to_vec()),
            Reply::Int(0),
        ]);
        assert!(classify_pubsub(r).is_err());
    }

    #[test]
    fn classify_rejects_wrong_arity() {
        let r = Reply::Array(vec![Reply::Bulk(b"subscribe".to_vec()), Reply::Bulk(b"x".to_vec())]);
        assert!(classify_pubsub(r).is_err());
    }
}
