//! Async mirror of `kevy_client::Subscriber` — TCP-only.
//!
//! A subscribed RESP connection cannot send normal commands, so this
//! is a separate type from [`crate::AsyncConnection`] (matching the
//! blocking client's split).
//!
//! What is NOT mirrored from the blocking surface:
//! - `set_read_timeout`: in async land timeouts are runtime-level
//!   (`tokio::time::timeout`, `async_io::Timer`); a socket-level
//!   `SO_RCVTIMEO` makes no sense when the read itself is non-blocking.
//!   Wrap a `recv()` future with your runtime's timeout primitive.
//! - `events()` / `messages()` blocking iterators: the async-native
//!   shape is a `Stream`, deferred to a future iteration. For now
//!   loop `recv().await` / `recv_message().await` directly.
//! - `mem://` / `file://` embed schemes: already rejected by the URL
//!   parser; embedded pub/sub is in-process synchronous, blocking
//!   client is strictly faster.

use std::io;

use kevy_resp::Reply;

use crate::codec::AsyncRespCodec;
use crate::pubsub::{PubsubEvent, classify};
use crate::url::parse_url;

#[cfg(feature = "tokio")]
type DefaultTransport = tokio::net::TcpStream;
#[cfg(feature = "smol")]
type DefaultTransport = smol::net::TcpStream;
#[cfg(feature = "async-std")]
type DefaultTransport = async_std::net::TcpStream;

#[cfg(feature = "tokio")]
async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_tokio::connect(host, port).await
}
#[cfg(feature = "smol")]
async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_smol::connect(host, port).await
}
#[cfg(feature = "async-std")]
async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_async_std::connect(host, port).await
}

/// Subscribed async TCP-RESP connection. Mirrors
/// [`kevy_client::Subscriber`] for TCP backends.
pub struct AsyncSubscriber {
    codec: AsyncRespCodec<DefaultTransport>,
}

impl AsyncSubscriber {
    /// Open a fresh connection without subscribing yet. Call
    /// [`Self::subscribe`] / [`Self::psubscribe`] next.
    pub async fn connect(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let transport = connect_default(&parsed.host, parsed.port).await?;
        Ok(Self {
            codec: AsyncRespCodec::new(transport),
        })
    }

    /// Open and subscribe to one or more channels in one step.
    pub async fn open(url: &str, channels: &[&[u8]]) -> io::Result<Self> {
        if channels.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AsyncSubscriber::open needs ≥ 1 channel — use connect() for empty start",
            ));
        }
        let mut s = Self::connect(url).await?;
        s.subscribe(channels).await?;
        Ok(s)
    }

    /// `SUBSCRIBE channel [channel ...]`. Per-channel Subscribe acks
    /// arrive via [`Self::recv`].
    pub async fn subscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        if channels.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SUBSCRIBE needs ≥ 1 channel",
            ));
        }
        self.send_with_args(b"SUBSCRIBE", channels).await
    }

    /// `PSUBSCRIBE pattern [pattern ...]`.
    pub async fn psubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        if patterns.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PSUBSCRIBE needs ≥ 1 pattern",
            ));
        }
        self.send_with_args(b"PSUBSCRIBE", patterns).await
    }

    /// `UNSUBSCRIBE [channel ...]`. Empty list = unsubscribe all.
    pub async fn unsubscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        self.send_with_args(b"UNSUBSCRIBE", channels).await
    }

    /// `PUNSUBSCRIBE [pattern ...]`. Empty list = unsubscribe all.
    pub async fn punsubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        self.send_with_args(b"PUNSUBSCRIBE", patterns).await
    }

    /// Await the next pubsub frame. Connection close = `UnexpectedEof`.
    pub async fn recv(&mut self) -> io::Result<PubsubEvent> {
        let reply = self.codec.read_reply().await?;
        classify(reply)
    }

    /// Skip subscription-ack frames and return the next published
    /// `Message` / `Pmessage`. Returns `(channel, payload)`; for
    /// pattern matches `channel` is the concrete publish channel
    /// (the matched pattern is discarded — use [`Self::recv`] if you
    /// need it).
    pub async fn recv_message(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        loop {
            match self.recv().await? {
                PubsubEvent::Message { channel, payload }
                | PubsubEvent::Pmessage {
                    channel, payload, ..
                } => return Ok((channel, payload)),
                _ => continue,
            }
        }
    }

    /// Negotiate RESP3 on this connection (`HELLO 3`). Must run BEFORE
    /// any subscribe — Redis spec requires HELLO be the first command.
    /// Returns a synthetic [`PubsubEvent::Subscribe`] marker (matching
    /// the blocking client) so callers can pattern-match a uniform
    /// type.
    pub async fn hello3(&mut self) -> io::Result<PubsubEvent> {
        let reply = self
            .codec
            .request(&[b"HELLO".to_vec(), b"3".to_vec()])
            .await?;
        match reply {
            Reply::Map(_) | Reply::Array(_) => Ok(PubsubEvent::Subscribe {
                channel: b"HELLO".to_vec(),
                count: 3,
            }),
            Reply::Error(e) => Err(io::Error::other(
                String::from_utf8_lossy(&e).into_owned(),
            )),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected HELLO 3 reply shape: {other:?}"),
            )),
        }
    }

    async fn send_with_args(&mut self, verb: &[u8], args: &[&[u8]]) -> io::Result<()> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(verb.to_vec());
        argv.extend(args.iter().map(|a| a.to_vec()));
        self.codec.send(&argv).await
    }
}
