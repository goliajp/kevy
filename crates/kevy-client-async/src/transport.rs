//! Async IO traits — runtime-agnostic core.
//!
//! Per RFC F5 (locked) the core of `kevy-client-async` depends only on
//! `core::future` / `core::task` / `std::io`. We do NOT pull `futures-io`,
//! `tokio::io::AsyncRead`, nor any other crate's IO traits — the
//! ecosystem's three runtimes each define their own near-identical
//! `AsyncRead` / `AsyncWrite`, and binding to any one of them would
//! bleed that runtime's dep through the core.
//!
//! Instead this module defines the traits ourselves in the
//! `futures-io` shape (poll-based, `&mut [u8]` buffers, returns
//! `Poll<io::Result<usize>>`). The runtime feature modules T4.5/T4.6/
//! T4.7 each ship a tiny adapter that implements these traits on top
//! of `<runtime>::net::TcpStream`.
//!
//! `AsyncTransport` is the bound the RESP3 codec (T4.4) and connection
//! type (T4.9) actually require: a single `AsyncRead + AsyncWrite +
//! Send + Unpin` thing. Blanket-impl'd for any qualifying type so
//! callers can hand in any compatible transport.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

/// Async equivalent of [`std::io::Read`] — poll-based, owned-buffer.
///
/// Implementors return `Poll::Pending` to register a waker and resume
/// when bytes become readable. `0` bytes returned from `Poll::Ready(Ok(0))`
/// signals clean EOF, mirroring the blocking semantics.
pub trait AsyncRead {
    /// Attempt to read bytes into `buf`. Returns the number of bytes
    /// written, or `Pending` if the underlying transport has nothing
    /// available yet.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>>;
}

/// Async equivalent of [`std::io::Write`] — poll-based, owned-buffer.
pub trait AsyncWrite {
    /// Attempt to write bytes from `buf`. Returns the number of bytes
    /// accepted, or `Pending`.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>>;

    /// Attempt to flush buffered bytes to the transport.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    /// Initiate / continue a graceful shutdown of the write half.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

/// Bound used everywhere downstream: codec (T4.4), `AsyncConnection`
/// (T4.9), pipeline runner (T4.16). Blanket-impl'd so any
/// `AsyncRead + AsyncWrite + Send + Unpin` value satisfies it.
pub trait AsyncTransport: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> AsyncTransport for T where T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized {}

// ─── Small read/write helpers built on the poll traits ────────────────
//
// The codec consumes bytes one chunk at a time. Rather than have every
// codec call site write its own poll-loop, expose a couple of futures
// that turn `poll_read` / `poll_write` into `.await`-able primitives.
// Both are zero-allocation — they borrow the transport + the buffer.

/// Future returned by [`read`]: drives a single `poll_read` to
/// completion.
pub struct Read<'a, T: ?Sized> {
    transport: &'a mut T,
    buf: &'a mut [u8],
}

/// Future returned by [`write_all`]: drives `poll_write` to completion
/// for the whole buffer (loops on partial writes internally).
pub struct WriteAll<'a, T: ?Sized> {
    transport: &'a mut T,
    buf: &'a [u8],
    written: usize,
}

/// Single-chunk async read. Resolves to the number of bytes read; `0`
/// = clean EOF.
pub fn read<'a, T>(transport: &'a mut T, buf: &'a mut [u8]) -> Read<'a, T>
where
    T: AsyncRead + Unpin + ?Sized,
{
    Read { transport, buf }
}

/// Async equivalent of `Write::write_all` — succeeds only after every
/// byte in `buf` is accepted by the transport.
pub fn write_all<'a, T>(transport: &'a mut T, buf: &'a [u8]) -> WriteAll<'a, T>
where
    T: AsyncWrite + Unpin + ?Sized,
{
    WriteAll {
        transport,
        buf,
        written: 0,
    }
}

impl<T> Future for Read<'_, T>
where
    T: AsyncRead + Unpin + ?Sized,
{
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        Pin::new(&mut *me.transport).poll_read(cx, me.buf)
    }
}

impl<T> Future for WriteAll<'_, T>
where
    T: AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        while me.written < me.buf.len() {
            let rem = &me.buf[me.written..];
            match Pin::new(&mut *me.transport).poll_write(cx, rem) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "transport accepted zero bytes",
                    )));
                }
                Poll::Ready(Ok(n)) => me.written += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}
