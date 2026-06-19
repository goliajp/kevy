//! Tokio runtime adapter — implements [`crate::transport::AsyncRead`]
//! / [`crate::transport::AsyncWrite`] on top of `tokio::net::TcpStream`.
//!
//! Activated by the `tokio` Cargo feature. See the crate-level docs
//! for the dep-rule exemption rationale.
//!
//! Why an adapter at all: tokio's own `AsyncRead` uses a `ReadBuf`
//! (initialised-byte tracking, useful for IO_uring); our trait uses a
//! plain `&mut [u8]`. The adapter is a 3-line shim — no buffering, no
//! extra allocation, no syscall.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::transport::{AsyncRead, AsyncWrite};

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut rb = ReadBuf::new(buf);
        match <Self as TokioAsyncRead>::poll_read(self, cx, &mut rb) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        <Self as TokioAsyncWrite>::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        <Self as TokioAsyncWrite>::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // tokio's equivalent is `poll_shutdown`.
        <Self as TokioAsyncWrite>::poll_shutdown(self, cx)
    }
}

/// Connect a tokio `TcpStream` to `host:port`, enabling
/// `TCP_NODELAY` (best-effort), and return it ready to feed into
/// [`crate::AsyncRespCodec::new`].
pub async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}
