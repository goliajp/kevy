//! The pubsub frame vocabulary shared by every kevy client crate and the
//! embedded engine's in-process bus: [`PubsubEvent`] (one received frame,
//! acks and deliveries alike)
//! and its `TryFrom<Reply>` (RESP reply → event, handling both RESP2
//! `*N` arrays and RESP3 `>N` push frames).

use std::io;

use crate::Reply;

/// One pubsub frame received from the bus or the wire.
///
/// `Unsubscribe` / `Punsubscribe`'s `channel` / `pattern` is `None`
/// when the server acknowledges "unsubscribed from everything" with a
/// nil bulk — matching the Redis wire shape.
///
/// ```
/// use kevy_resp::{PubsubEvent, parse_reply};
///
/// let (reply, _) = parse_reply(b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?
///     .expect("complete frame");
/// match PubsubEvent::try_from(reply)? {
///     PubsubEvent::Message { channel, payload } => {
///         assert_eq!((channel, payload), (b"news".to_vec(), b"hi".to_vec()));
///     }
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubsubEvent {
    /// `SUBSCRIBE` ack per channel.
    ///
    /// ```
    /// use kevy_resp::{PubsubEvent, parse_reply};
    /// let (reply, _) = parse_reply(b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")?.expect("complete frame");
    /// let event = PubsubEvent::try_from(reply)?;
    /// assert_eq!(event, PubsubEvent::Subscribe { channel: b"news".to_vec(), count: 1 });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Subscribe {
        /// Channel that was just subscribed.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Subscribe { channel, .. } = event else { panic!("not subscribe") };
        /// assert_eq!(channel, b"news");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        channel: Vec<u8>,
        /// Total channels + patterns subscribed.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Subscribe { count, .. } = event else { panic!("not subscribe") };
        /// assert_eq!(count, 1);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        count: i64,
    },
    /// `PSUBSCRIBE` ack per pattern.
    ///
    /// ```
    /// use kevy_resp::{PubsubEvent, parse_reply};
    /// let (reply, _) = parse_reply(b"*3\r\n$10\r\npsubscribe\r\n$3\r\nn.*\r\n:2\r\n")?.expect("complete frame");
    /// let event = PubsubEvent::try_from(reply)?;
    /// assert_eq!(event, PubsubEvent::Psubscribe { pattern: b"n.*".to_vec(), count: 2 });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Psubscribe {
        /// Pattern that was just subscribed.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$10\r\npsubscribe\r\n$3\r\nn.*\r\n:2\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Psubscribe { pattern, .. } = event else { panic!("not psubscribe") };
        /// assert_eq!(pattern, b"n.*");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        pattern: Vec<u8>,
        /// Total channels + patterns subscribed.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$10\r\npsubscribe\r\n$3\r\nn.*\r\n:2\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// // the connection now holds one channel and this pattern
        /// let PubsubEvent::Psubscribe { count, .. } = event else { panic!("not psubscribe") };
        /// assert_eq!(count, 2);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        count: i64,
    },
    /// `UNSUBSCRIBE` ack.
    ///
    /// ```
    /// use kevy_resp::{PubsubEvent, parse_reply};
    /// let (reply, _) = parse_reply(b"*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:0\r\n")?.expect("complete frame");
    /// let event = PubsubEvent::try_from(reply)?;
    /// assert_eq!(event, PubsubEvent::Unsubscribe { channel: Some(b"news".to_vec()), count: 0 });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Unsubscribe {
        /// Channel just unsubscribed (`None` for "all"/"none" nil bulk).
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// // "unsubscribed from everything" arrives as a nil bulk
        /// let PubsubEvent::Unsubscribe { channel, .. } = event else { panic!("not unsubscribe") };
        /// assert_eq!(channel, None);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        channel: Option<Vec<u8>>,
        /// Total channels + patterns still subscribed.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:0\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Unsubscribe { count, .. } = event else { panic!("not unsubscribe") };
        /// assert_eq!(count, 0);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        count: i64,
    },
    /// `PUNSUBSCRIBE` ack.
    ///
    /// ```
    /// use kevy_resp::{PubsubEvent, parse_reply};
    /// let (reply, _) = parse_reply(b"*3\r\n$12\r\npunsubscribe\r\n$3\r\nn.*\r\n:1\r\n")?.expect("complete frame");
    /// let event = PubsubEvent::try_from(reply)?;
    /// assert_eq!(event, PubsubEvent::Punsubscribe { pattern: Some(b"n.*".to_vec()), count: 1 });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Punsubscribe {
        /// Pattern just unsubscribed (`None` for "all"/"none" nil bulk).
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$12\r\npunsubscribe\r\n$-1\r\n:0\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Punsubscribe { pattern, .. } = event else { panic!("not punsubscribe") };
        /// assert_eq!(pattern, None);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        pattern: Option<Vec<u8>>,
        /// Total channels + patterns still subscribed.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*3\r\n$12\r\npunsubscribe\r\n$3\r\nn.*\r\n:1\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// // one channel subscription is still active
        /// let PubsubEvent::Punsubscribe { count, .. } = event else { panic!("not punsubscribe") };
        /// assert_eq!(count, 1);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        count: i64,
    },
    /// Plain `PUBLISH` delivery on a subscribed channel.
    ///
    /// ```
    /// use kevy_resp::{PubsubEvent, parse_reply};
    /// // a RESP3 push frame
    /// let (reply, _) = parse_reply(b">3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
    /// let event = PubsubEvent::try_from(reply)?;
    /// assert_eq!(event, PubsubEvent::Message { channel: b"news".to_vec(), payload: b"hi".to_vec() });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Message {
        /// Channel the publish was made to.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b">3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Message { channel, .. } = event else { panic!("not message") };
        /// assert_eq!(channel, b"news");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        channel: Vec<u8>,
        /// Raw payload bytes.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b">3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// assert_eq!(event.into_payload(), Some(b"hi".to_vec()));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        payload: Vec<u8>,
    },
    /// Pattern-match delivery.
    ///
    /// ```
    /// use kevy_resp::{PubsubEvent, parse_reply};
    /// let (reply, _) = parse_reply(b"*4\r\n$8\r\npmessage\r\n$3\r\nn.*\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
    /// let event = PubsubEvent::try_from(reply)?;
    /// let expected = PubsubEvent::Pmessage {
    ///     pattern: b"n.*".to_vec(),
    ///     channel: b"news".to_vec(),
    ///     payload: b"hi".to_vec(),
    /// };
    /// assert_eq!(event, expected);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Pmessage {
        /// Pattern the channel matched.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*4\r\n$8\r\npmessage\r\n$3\r\nn.*\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Pmessage { pattern, .. } = event else { panic!("not pmessage") };
        /// assert_eq!(pattern, b"n.*");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        pattern: Vec<u8>,
        /// Channel the publish was made to.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*4\r\n$8\r\npmessage\r\n$3\r\nn.*\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Pmessage { channel, .. } = event else { panic!("not pmessage") };
        /// assert_eq!(channel, b"news");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        channel: Vec<u8>,
        /// Raw payload bytes.
        ///
        /// ```
        /// use kevy_resp::{PubsubEvent, parse_reply};
        /// let (reply, _) = parse_reply(b"*4\r\n$8\r\npmessage\r\n$3\r\nn.*\r\n$4\r\nnews\r\n$2\r\nhi\r\n")?.expect("complete frame");
        /// let event = PubsubEvent::try_from(reply)?;
        /// let PubsubEvent::Pmessage { payload, .. } = event else { panic!("not pmessage") };
        /// assert_eq!(payload, b"hi");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        payload: Vec<u8>,
    },
}

/// Turn a RESP reply into a [`PubsubEvent`]. Handles both RESP2
/// (`*N\r\n…` arrays) and RESP3 (`>N\r\n…` push frames).
///
/// ```
/// use kevy_resp::{PubsubEvent, Reply};
///
/// let r = Reply::Array(vec![
///     Reply::Bulk(b"message".to_vec()),
///     Reply::Bulk(b"news".to_vec()),
///     Reply::Bulk(b"hi".to_vec()),
/// ]);
/// assert!(matches!(PubsubEvent::try_from(r)?, PubsubEvent::Message { .. }));
/// # Ok::<(), std::io::Error>(())
/// ```
impl PubsubEvent {
    /// The raw message payload, moved out of the event.
    ///
    /// `Some(payload)` for the two deliveries ([`Message`](Self::Message)
    /// and [`Pmessage`](Self::Pmessage)); `None` for every control/ack
    /// event (subscribe / unsubscribe / …), which carries no payload.
    /// Consuming `self` lets a scalar drain hand a push subscriber just the
    /// bytes with no extra copy. The channel and the message-vs-pmessage
    /// distinction are dropped; a caller that needs either keeps matching
    /// on the event.
    ///
    /// ```
    /// use kevy_resp::PubsubEvent;
    ///
    /// let m = PubsubEvent::Message { channel: b"c".to_vec(), payload: b"p".to_vec() };
    /// assert_eq!(m.into_payload(), Some(b"p".to_vec()));
    /// let ack = PubsubEvent::Subscribe { channel: b"c".to_vec(), count: 1 };
    /// assert_eq!(ack.into_payload(), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn into_payload(self) -> Option<Vec<u8>> {
        match self {
            PubsubEvent::Message { payload, .. } | PubsubEvent::Pmessage { payload, .. } => {
                Some(payload)
            }
            _ => None,
        }
    }
}

impl TryFrom<Reply> for PubsubEvent {
    type Error = io::Error;

    // LOC-WAIVER: data-driven pubsub-kind match table — one flat frame-destructure arm per kind.
    fn try_from(reply: Reply) -> io::Result<PubsubEvent> {
        let items = match reply {
            Reply::Array(v) | Reply::Push(v) => v,
            Reply::Error(e) => {
                return Err(io::Error::other(String::from_utf8_lossy(&e).into_owned()));
            }
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
            other => {
                Err(invalid(format!("unknown pubsub kind: {}", String::from_utf8_lossy(other))))
            }
        }
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
#[path = "pubsub_event_tests.rs"]
mod tests;
