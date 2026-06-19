//! Async equivalent of [`kevy_client::Connection`] — TCP-only.
//!
//! Drop-in mirror: the migration path from blocking is grep-replace
//! `Connection` → `AsyncConnection` plus `.await` on each call.
//!
//! The active transport type is picked at compile-time from whichever
//! runtime feature is enabled (T4.8 ensures exactly one). The codec
//! is generic over `AsyncTransport` so this just type-alises the
//! runtime-specific TcpStream.

use std::io;

use kevy_resp::Reply;

use crate::codec::AsyncRespCodec;
use crate::url::parse_url;

// ─── Runtime-selected default transport ───────────────────────────────
//
// T4.8 guarantees exactly one of the three feature blocks below is
// active, so `DefaultTransport` is unambiguously defined.

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

// ─── AsyncConnection ──────────────────────────────────────────────────

/// Async TCP-RESP connection. Mirrors [`kevy_client::Connection`] but
/// drops the `mem://` / `file://` embedded backends — those are
/// synchronous and have no async story.
pub struct AsyncConnection {
    codec: AsyncRespCodec<DefaultTransport>,
}

impl AsyncConnection {
    /// Open a connection from a URL. Accepts `kevy://`, `redis://`,
    /// `tcp://` — see [`crate::url::parse_url`] for the full grammar.
    ///
    /// If the URL carries a `/N` db index (only `kevy://` and `redis://`),
    /// an initial `SELECT N` round-trip runs before returning.
    pub async fn open(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let transport = connect_default(&parsed.host, parsed.port).await?;
        let mut codec = AsyncRespCodec::new(transport);
        if let Some(db) = parsed.db {
            let reply = codec
                .request(&[b"SELECT".to_vec(), db.to_string().into_bytes()])
                .await?;
            if let Reply::Error(msg) = reply {
                let text = String::from_utf8_lossy(&msg);
                return Err(io::Error::other(format!("SELECT {db} rejected: {text}")));
            }
        }
        Ok(Self { codec })
    }

    /// Direct constructor — useful when the caller wants to manage
    /// transport setup itself (cluster client, custom socket opts).
    pub fn from_transport(transport: DefaultTransport) -> Self {
        Self {
            codec: AsyncRespCodec::new(transport),
        }
    }

    /// `PING`. Returns `Ok(())` on `+PONG`.
    pub async fn ping(&mut self) -> io::Result<()> {
        let reply = self.codec.request(&[b"PING".to_vec()]).await?;
        expect_pong(reply)
    }

    /// Borrow the underlying codec — exposed so pipeline + subscriber
    /// adapters built in later tasks (T4.11/T4.14) can share the
    /// connection state machine.
    pub fn codec_mut(&mut self) -> &mut AsyncRespCodec<DefaultTransport> {
        &mut self.codec
    }
}

fn expect_pong(reply: Reply) -> io::Result<()> {
    match reply {
        Reply::Simple(s) if s == b"PONG" => Ok(()),
        Reply::Bulk(s) if s == b"PONG" => Ok(()),
        Reply::Error(msg) => Err(io::Error::other(format!(
            "PING failed: {}",
            String::from_utf8_lossy(&msg)
        ))),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PING returned unexpected reply: {other:?}"),
        )),
    }
}
