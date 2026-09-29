//! Async equivalent of [`kevy_client::Connection`](https://docs.rs/kevy-client/latest/kevy_client/enum.Connection.html) — TCP-only.
//!
//! Drop-in mirror: the migration path from blocking is grep-replace
//! `Connection` → `AsyncConnection` plus `.await` on each call.
//!
//! The active transport type is picked at compile-time from whichever
//! runtime feature is enabled (the crate-level feature gate enforces
//! exactly one via `compile_error!`). The codec
//! is generic over `AsyncTransport` so this just type-alises the
//! runtime-specific TcpStream.

use std::io;

use kevy_resp::Reply;

use crate::AsyncSecure;
use crate::codec::AsyncRespCodec;
use crate::url::parse_url;

// ─── Runtime-selected default transport ───────────────────────────────
//
// The crate-level compile_error! gate guarantees exactly one of the
// three feature blocks below is active, so `DefaultTransport` is
// unambiguously defined.

#[cfg(feature = "tokio")]
pub(crate) type DefaultTransport = tokio::net::TcpStream;
#[cfg(feature = "smol")]
pub(crate) type DefaultTransport = smol::net::TcpStream;
#[cfg(feature = "async-std")]
pub(crate) type DefaultTransport = async_std::net::TcpStream;

#[cfg(feature = "tokio")]
pub(crate) async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_tokio::connect(host, port).await
}
#[cfg(feature = "smol")]
pub(crate) async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_smol::connect(host, port).await
}
#[cfg(feature = "async-std")]
pub(crate) async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_async_std::connect(host, port).await
}

// ─── AsyncConnection ──────────────────────────────────────────────────

/// Async TCP-RESP connection. Mirrors [`kevy_client::Connection`](https://docs.rs/kevy-client/latest/kevy_client/enum.Connection.html) but
/// drops the `mem://` / `file://` embedded backends — those are
/// synchronous and have no async story.
///
/// The transport defaults to the runtime's `TcpStream`;
/// [`Self::connect_secure_url`] gives one over [`AsyncSecure`] instead.
#[derive(Debug)]
pub struct AsyncConnection<T = DefaultTransport> {
    codec: AsyncRespCodec<T>,
}

impl AsyncConnection {
    /// Connect from a URL. Accepts `kevy://`, `redis://`,
    /// `tcp://` — see [`crate::url::parse_url`] for the full grammar.
    ///
    /// If the URL carries a `/N` db index (only `kevy://` and `redis://`),
    /// an initial `SELECT N` round-trip runs before returning.
    pub async fn connect(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let transport = connect_default(&parsed.host, parsed.port).await?;
        let mut codec = AsyncRespCodec::new(transport);
        if let Some(db) = parsed.db {
            let reply = codec.request(&[b"SELECT".to_vec(), db.to_string().into_bytes()]).await?;
            if let Reply::Error(msg) = reply {
                let text = String::from_utf8_lossy(&msg);
                return Err(io::Error::other(format!("SELECT {db} rejected: {text}")));
            }
        }
        Ok(Self { codec })
    }
}

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// Direct constructor — useful when the caller wants to manage
    /// transport setup itself (custom socket options, a transport of its
    /// own that implements [`crate::AsyncRead`] and [`crate::AsyncWrite`]).
    ///
    /// ```no_run
    /// # #[cfg(feature = "tokio")]
    /// # async fn demo() -> std::io::Result<()> {
    /// let tcp = kevy_client_async::rt_tokio::connect("127.0.0.1", 6004).await?;
    /// let mut c = kevy_client_async::AsyncConnection::from_transport(tcp);
    /// c.ping().await?;
    /// # Ok(()) }
    /// ```
    pub fn from_transport(transport: T) -> Self {
        Self { codec: AsyncRespCodec::new(transport) }
    }
}

impl AsyncConnection<AsyncSecure<DefaultTransport>> {
    /// Connect to a server's encrypted client port:
    /// `kevys://host[:port][/db]?server_key=<hex>[&client_key_file=<path>]`.
    /// Every command method works the same as on a plaintext connection.
    ///
    /// ```no_run
    /// # async fn demo() -> std::io::Result<()> {
    /// use kevy_client_async::AsyncConnection;
    /// let url = format!("kevys://10.0.0.5:6404?server_key={}", "ab".repeat(32));
    /// let mut c = AsyncConnection::connect_secure_url(&url).await?;
    /// c.ping().await?;
    /// # Ok(()) }
    /// ```
    pub async fn connect_secure_url(url: &str) -> io::Result<Self> {
        let (transport, db) = connect_secure(url).await?;
        let mut codec = AsyncRespCodec::new(transport);
        if let Some(db) = db {
            let reply = codec.request(&[b"SELECT".to_vec(), db.to_string().into_bytes()]).await?;
            if let Reply::Error(msg) = reply {
                let text = String::from_utf8_lossy(&msg);
                return Err(io::Error::other(format!("SELECT {db} rejected: {text}")));
            }
        }
        Ok(Self { codec })
    }
}

/// Dial and handshake a `kevys://` URL; also returns its `/db`, if any.
pub(crate) async fn connect_secure(
    url: &str,
) -> io::Result<(AsyncSecure<DefaultTransport>, Option<u32>)> {
    let u = kevy_resp_client::SecureUrl::parse(url)?;
    let me = u.client_key_file.as_deref().map(kevy_resp_client::load_client_key).transpose()?;
    let tcp = connect_default(&u.host, u.port).await?;
    Ok((AsyncSecure::handshake(tcp, u.server_key, me.as_ref()).await?, u.db))
}

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// `PING`. Returns `Ok(())` on `+PONG`.
    pub async fn ping(&mut self) -> io::Result<()> {
        let reply = self.codec.request(&[b"PING".to_vec()]).await?;
        expect_pong(reply)
    }

    /// Borrow the underlying codec — exposed so the pipeline and
    /// subscriber adapters can share the connection state machine.
    pub fn codec_mut(&mut self) -> &mut AsyncRespCodec<T> {
        &mut self.codec
    }
}

fn expect_pong(reply: Reply) -> io::Result<()> {
    match reply {
        Reply::Simple(s) if s == b"PONG" => Ok(()),
        Reply::Bulk(s) if s == b"PONG" => Ok(()),
        Reply::Error(msg) => {
            Err(io::Error::other(format!("PING failed: {}", String::from_utf8_lossy(&msg))))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PING returned unexpected reply: {other:?}"),
        )),
    }
}
