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
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n")]).await?;
//! use kevy_client_async::subscriber::AsyncSubscriber;
//!
//! let mut s = AsyncSubscriber::connect_channels(&format!("kevy://{addr}"), &[b"news"]).await?;
//! let (channel, payload) = s.recv_message().await?;
//! assert_eq!((&channel[..], &payload[..]), (&b"news"[..], &b"hello"[..]));
//! # Ok(()) }
//! ```

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
///
/// ```
/// # include!("doc_serve.rs");
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> std::io::Result<()> {
/// # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n")]).await?;
/// use kevy_client_async::subscriber::AsyncSubscriber;
///
/// let mut s = AsyncSubscriber::connect_channels(&format!("kevy://{addr}"), &[b"news"]).await?;
/// // a message published to `news` after the subscription
/// assert_eq!(s.recv_message().await?, (b"news".to_vec(), b"hello".to_vec()));
/// # Ok(()) }
/// ```
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect(&format!("kevy://{addr}")).await?;
    /// s.subscribe(&[b"news"]).await?;
    /// assert_eq!(s.recv().await?, PubsubEvent::Subscribe { channel: b"news".to_vec(), count: 1 });
    /// # Ok(()) }
    /// ```
    pub async fn connect(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let transport = connect_default(&parsed.host, parsed.port).await?;
        Ok(Self {
            codec: AsyncRespCodec::new(transport),
            pending: std::collections::VecDeque::new(),
        })
    }

    /// Connect and subscribe to one or more channels in one step.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect_channels(&format!("kevy://{addr}"), &[b"news"]).await?;
    /// // subscribed already: the ack is the first event
    /// assert_eq!(s.recv().await?, PubsubEvent::Subscribe { channel: b"news".to_vec(), count: 1 });
    /// // at least one channel is required
    /// let err = AsyncSubscriber::connect_channels(&format!("kevy://{addr}"), &[]).await.unwrap_err();
    /// assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    /// # Ok(()) }
    /// ```
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
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let (addr, key) = serve_secure(&[("SUBSCRIBE news", concat!(
    /// #     "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n",
    /// #     "*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n",
    /// # ))]).await?;
    /// # let key = hex(&key);
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    ///
    /// let url = format!("kevys://{addr}?server_key={key}");
    /// let mut s = AsyncSubscriber::connect_secure_url(&url).await?;
    /// s.subscribe(&[b"news"]).await?;
    /// assert_eq!(s.recv_message().await?, (b"news".to_vec(), b"hi".to_vec()));
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect(&format!("kevy://{addr}")).await?;
    /// s.subscribe(&[b"news"]).await?; // resolves once `news` is acked
    /// // the ack itself stays in the event stream
    /// assert_eq!(s.recv().await?, PubsubEvent::Subscribe { channel: b"news".to_vec(), count: 1 });
    /// # Ok(()) }
    /// ```
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PSUBSCRIBE news.*", "*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n*4\r\n$8\r\npmessage\r\n$6\r\nnews.*\r\n$7\r\nnews.eu\r\n$2\r\nhi\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect(&format!("kevy://{addr}")).await?;
    /// s.psubscribe(&[b"news.*"]).await?;
    /// assert!(matches!(s.recv().await?, PubsubEvent::Psubscribe { count: 1, .. }));
    /// let ev = s.recv().await?;
    /// let PubsubEvent::Pmessage { pattern, channel, payload } = ev else { panic!("{ev:?}") };
    /// assert_eq!((&pattern[..], &channel[..], &payload[..]), (&b"news.*"[..], &b"news.eu"[..], &b"hi"[..]));
    /// # Ok(()) }
    /// ```
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n"), ("UNSUBSCRIBE news", "*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:0\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect_channels(&format!("kevy://{addr}"), &[b"news"]).await?;
    /// s.unsubscribe(&[b"news"]).await?;
    /// s.recv().await?; // the SUBSCRIBE ack
    /// let ack = s.recv().await?;
    /// assert_eq!(ack, PubsubEvent::Unsubscribe { channel: Some(b"news".to_vec()), count: 0 });
    /// # Ok(()) }
    /// ```
    pub async fn unsubscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        self.send_with_args(b"UNSUBSCRIBE", channels).await
    }

    /// `PUNSUBSCRIBE [pattern ...]`. Empty list = unsubscribe all.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PSUBSCRIBE news.*", "*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n"), ("PUNSUBSCRIBE", "*3\r\n$12\r\npunsubscribe\r\n$6\r\nnews.*\r\n:0\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect(&format!("kevy://{addr}")).await?;
    /// s.psubscribe(&[b"news.*"]).await?;
    /// s.punsubscribe(&[]).await?; // every pattern
    /// s.recv().await?; // the PSUBSCRIBE ack
    /// assert!(matches!(s.recv().await?, PubsubEvent::Punsubscribe { count: 0, .. }));
    /// # Ok(()) }
    /// ```
    pub async fn punsubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        self.send_with_args(b"PUNSUBSCRIBE", patterns).await
    }

    /// Await the next pubsub frame. Connection close = `UnexpectedEof`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SUBSCRIBE news", "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    /// use kevy_client_async::pubsub::PubsubEvent;
    ///
    /// let mut s = AsyncSubscriber::connect_channels(&format!("kevy://{addr}"), &[b"news"]).await?;
    /// assert!(matches!(s.recv().await?, PubsubEvent::Subscribe { .. }));
    /// let msg = PubsubEvent::Message { channel: b"news".to_vec(), payload: b"hello".to_vec() };
    /// assert_eq!(s.recv().await?, msg);
    /// # Ok(()) }
    /// ```
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PSUBSCRIBE news.*", "*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n*4\r\n$8\r\npmessage\r\n$6\r\nnews.*\r\n$7\r\nnews.eu\r\n$2\r\nhi\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    ///
    /// let mut s = AsyncSubscriber::connect(&format!("kevy://{addr}")).await?;
    /// s.psubscribe(&[b"news.*"]).await?;
    /// // the ack is skipped; for a pattern match the concrete channel comes back
    /// assert_eq!(s.recv_message().await?, (b"news.eu".to_vec(), b"hi".to_vec()));
    /// # Ok(()) }
    /// ```
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HELLO 3", "%1\r\n$6\r\nserver\r\n$4\r\nkevy\r\n"), ("SUBSCRIBE news", ">3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n")]).await?;
    /// use kevy_client_async::subscriber::AsyncSubscriber;
    ///
    /// let mut s = AsyncSubscriber::connect(&format!("kevy://{addr}")).await?;
    /// s.hello3().await?; // before any subscribe
    /// s.subscribe(&[b"news"]).await?; // RESP3 push frames from here on
    /// # Ok(()) }
    /// ```
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
