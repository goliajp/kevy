//! `PubsubEvent` enum — mirror of the type in
//! `kevy_client::subscribe::PubsubEvent`. Lives here so the async
//! subscriber doesn't have to dep on the blocking client crate.
//!
//! TODO: this is the third PubsubEvent in the workspace shape-wise
//! (kevy-embedded's `PubsubFrame`, `kevy-client::subscribe::PubsubEvent`,
//! and this one). When the fourth client lands, lift this and the
//! RESP-shape classifier into a `kevy-pubsub` stone — same trigger as
//! the `kevy-url` deferral noted in `url.rs`.

use std::io;

use kevy_resp::Reply;

/// One pubsub frame received over the wire.
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
pub(crate) fn classify(reply: Reply) -> io::Result<PubsubEvent> {
    let items = match reply {
        Reply::Array(v) | Reply::Push(v) => v,
        Reply::Error(e) => return Err(io::Error::other(String::from_utf8_lossy(&e).into_owned())),
        other => {
            return Err(invalid(format!(
                "pubsub: expected array/push, got {}",
                shape(&other)
            )));
        }
    };

    let mut it = items.into_iter();
    let kind = take_bulk(
        it.next().ok_or_else(|| invalid("pubsub: empty frame"))?,
        "kind",
    )?;

    match kind.as_slice() {
        b"subscribe" => {
            let channel = take_bulk(
                it.next().ok_or_else(|| invalid("subscribe: missing channel"))?,
                "channel",
            )?;
            let count = take_int(
                it.next().ok_or_else(|| invalid("subscribe: missing count"))?,
                "count",
            )?;
            Ok(PubsubEvent::Subscribe { channel, count })
        }
        b"psubscribe" => {
            let pattern = take_bulk(
                it.next().ok_or_else(|| invalid("psubscribe: missing pattern"))?,
                "pattern",
            )?;
            let count = take_int(
                it.next().ok_or_else(|| invalid("psubscribe: missing count"))?,
                "count",
            )?;
            Ok(PubsubEvent::Psubscribe { pattern, count })
        }
        b"unsubscribe" => {
            let channel = take_bulk_or_nil(
                it.next().ok_or_else(|| invalid("unsubscribe: missing channel"))?,
                "channel",
            )?;
            let count = take_int(
                it.next().ok_or_else(|| invalid("unsubscribe: missing count"))?,
                "count",
            )?;
            Ok(PubsubEvent::Unsubscribe { channel, count })
        }
        b"punsubscribe" => {
            let pattern = take_bulk_or_nil(
                it.next()
                    .ok_or_else(|| invalid("punsubscribe: missing pattern"))?,
                "pattern",
            )?;
            let count = take_int(
                it.next()
                    .ok_or_else(|| invalid("punsubscribe: missing count"))?,
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
            Ok(PubsubEvent::Pmessage {
                pattern,
                channel,
                payload,
            })
        }
        other => Err(invalid(format!(
            "unknown pubsub kind: {}",
            String::from_utf8_lossy(other)
        ))),
    }
}

fn take_bulk(r: Reply, field: &str) -> io::Result<Vec<u8>> {
    match r {
        Reply::Bulk(v) | Reply::Simple(v) => Ok(v),
        other => Err(invalid(format!(
            "pubsub field {field}: expected bulk, got {}",
            shape(&other)
        ))),
    }
}

fn take_bulk_or_nil(r: Reply, field: &str) -> io::Result<Option<Vec<u8>>> {
    match r {
        Reply::Bulk(v) | Reply::Simple(v) => Ok(Some(v)),
        Reply::Nil | Reply::Null => Ok(None),
        other => Err(invalid(format!(
            "pubsub field {field}: expected bulk/nil, got {}",
            shape(&other)
        ))),
    }
}

fn take_int(r: Reply, field: &str) -> io::Result<i64> {
    match r {
        Reply::Int(n) => Ok(n),
        other => Err(invalid(format!(
            "pubsub field {field}: expected int, got {}",
            shape(&other)
        ))),
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
