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
use crate::pubsub::PubsubEvent;
use crate::url::parse_url;

use crate::conn::{DefaultTransport, connect_default};

/// Subscribed async TCP-RESP connection. Mirrors
/// [`kevy_client::Subscriber`](https://docs.rs/kevy-client/latest/kevy_client/struct.Subscriber.html) for TCP backends.
///
/// The transport defaults to the runtime's `TcpStream`;
/// [`Self::connect_secure_url`] gives one over [`crate::AsyncSecure`].
#[derive(Debug)]
pub struct AsyncSubscriber<T = DefaultTransport> {
    codec: AsyncRespCodec<T>,
    /// Events read while waiting for a subscribe ack, in arrival order.
    /// See [`Self::subscribe`].
    pending: std::collections::VecDeque<PubsubEvent>,
}

impl AsyncSubscriber {
    /// Open a fresh connection without subscribing yet. Call
    /// [`Self::subscribe`] / [`Self::psubscribe`] next.
    pub async fn connect(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let transport = connect_default(&parsed.host, parsed.port).await?;
        Ok(Self {
            codec: AsyncRespCodec::new(transport),
            pending: std::collections::VecDeque::new(),
        })
    }

    /// Connect and subscribe to one or more channels in one step.
    pub async fn connect_channels(url: &str, channels: &[&[u8]]) -> io::Result<Self> {
        if channels.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AsyncSubscriber::connect_channels needs ≥ 1 channel — use connect() for empty start",
            ));
        }
        let mut s = Self::connect(url).await?;
        s.subscribe(channels).await?;
        Ok(s)
    }
}

impl AsyncSubscriber<crate::AsyncSecure<DefaultTransport>> {
    /// [`AsyncSubscriber::connect`] for a `kevys://` URL: the same
    /// subscriber, over the server's encrypted client port.
    ///
    /// ```no_run
    /// # async fn demo() -> std::io::Result<()> {
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// let url = format!("kevys://10.0.0.5:6404?server_key={}", "ab".repeat(32));
    /// let mut s = AsyncSubscriber::connect_secure_url(&url).await?;
    /// s.subscribe(&[b"news"]).await?;
    /// # Ok(()) }
    /// ```
    pub async fn connect_secure_url(url: &str) -> io::Result<Self> {
        let (transport, _) = crate::conn::connect_secure(url).await?;
        Ok(Self {
            codec: AsyncRespCodec::new(transport),
            pending: std::collections::VecDeque::new(),
        })
    }
}

impl<T: crate::AsyncTransport> AsyncSubscriber<T> {
    /// `SUBSCRIBE channel [channel ...]`. Returns once the server has
    /// acked every channel — you are subscribed when this resolves.
    ///
    /// The acks are still delivered through [`Self::recv`]: everything
    /// read while waiting is QUEUED, not consumed, so the observable
    /// event stream is unchanged. A message for an already-subscribed
    /// channel can arrive before the ack for a new one, and dropping it
    /// to get at the ack would trade a race for a lost message.
    pub async fn subscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        if channels.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "SUBSCRIBE needs ≥ 1 channel"));
        }
        self.send_with_args(b"SUBSCRIBE", channels).await?;
        self.await_acks(channels.len(), false).await
    }

    /// Read until `n` acks have arrived, queueing everything else.
    async fn await_acks(&mut self, n: usize, want_pattern: bool) -> io::Result<()> {
        let mut seen = 0usize;
        while seen < n {
            let ev = PubsubEvent::try_from(self.codec.read_reply().await?)?;
            let is_ack = if want_pattern {
                matches!(ev, PubsubEvent::Psubscribe { .. })
            } else {
                matches!(ev, PubsubEvent::Subscribe { .. })
            };
            if is_ack {
                seen += 1;
            }
            self.pending.push_back(ev);
        }
        Ok(())
    }

    /// `PSUBSCRIBE pattern [pattern ...]`.
    pub async fn psubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        if patterns.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PSUBSCRIBE needs ≥ 1 pattern",
            ));
        }
        self.send_with_args(b"PSUBSCRIBE", patterns).await?;
        self.await_acks(patterns.len(), true).await
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
        if let Some(ev) = self.pending.pop_front() {
            return Ok(ev);
        }
        let reply = self.codec.read_reply().await?;
        PubsubEvent::try_from(reply)
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
                | PubsubEvent::Pmessage { channel, payload, .. } => return Ok((channel, payload)),
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
        let reply = self.codec.request(&[b"HELLO".to_vec(), b"3".to_vec()]).await?;
        match reply {
            Reply::Map(_) | Reply::Array(_) => {
                Ok(PubsubEvent::Subscribe { channel: b"HELLO".to_vec(), count: 3 })
            }
            Reply::Error(e) => Err(io::Error::other(String::from_utf8_lossy(&e).into_owned())),
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
