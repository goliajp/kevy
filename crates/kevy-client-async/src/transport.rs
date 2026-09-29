//! Async IO traits — runtime-agnostic core.
//!
//! The core of `kevy-client-async` depends only on
//! `core::future` / `core::task` / `std::io`. We do NOT pull `futures-io`,
//! `tokio::io::AsyncRead`, nor any other crate's IO traits — the
//! ecosystem's three runtimes each define their own near-identical
//! `AsyncRead` / `AsyncWrite`, and binding to any one of them would
//! bleed that runtime's dep through the core.
//!
//! Instead this module defines the traits ourselves in the
//! `futures-io` shape (poll-based, `&mut [u8]` buffers, returns
//! `Poll<io::Result<usize>>`). The per-runtime feature modules
//! (`rt_tokio` / `rt_smol` / `rt_async_std`) each ship a tiny adapter
//! that implements these traits on top of `<runtime>::net::TcpStream`.
//!
//! `AsyncTransport` is the bound the RESP3 codec and connection type
//! actually require: a single `AsyncRead + AsyncWrite + Send + Unpin`
//! thing. Blanket-impl'd for any qualifying type so callers can hand
//! in any compatible transport.
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
//! # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
//! # let (mut b, _) = listener.accept().await?;
//! // `a` and `b` are the two ends of a TCP connection
//! use kevy_client_async::{read, write_all};
//!
//! write_all(&mut a, b"*1\r\n$4\r\nPING\r\n").await?;
//! let mut buf = [0u8; 64];
//! let n = read(&mut b, &mut buf).await?;
//! assert_eq!(&buf[..n], b"*1\r\n$4\r\nPING\r\n");
//! # Ok(()) }
//! ```

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

/// Async equivalent of [`std::io::Read`] — poll-based, owned-buffer.
///
/// Implement it (with [`AsyncWrite`]) to run the client over a transport
/// of your own — a Unix socket, an in-memory pipe, another runtime's
/// stream — and hand it to [`crate::AsyncRespCodec::new`],
/// [`crate::AsyncConnection::from_transport`] or
/// [`crate::AsyncSecure::handshake`].
///
/// # Contract
///
/// - `Poll::Pending` means nothing is readable yet, and the waker in `cx`
///   has been registered to fire once bytes (or EOF, or an error) arrive;
///   returning `Pending` without registering it stalls the client forever.
/// - `Poll::Ready(Ok(n))` with `n > 0` means the first `n` bytes of `buf`
///   now hold the next bytes of the stream, in order; `n <= buf.len()`.
/// - `Poll::Ready(Ok(0))` for a non-empty `buf` is clean end of stream,
///   as with blocking `Read`; the client treats it as the peer closing.
/// - Errors are `io::Error`s whose kind the caller may match
///   (`UnexpectedEof`, `ConnectionReset`, …); `Interrupted` is not retried.
///
/// ```
/// use core::pin::Pin;
/// use core::task::{Context, Poll};
/// use kevy_client_async::AsyncRead;
///
/// /// A transport that replays fixed bytes, then reports end of stream.
/// struct Canned(&'static [u8]);
///
/// impl AsyncRead for Canned {
///     fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
///         let n = self.0.len().min(buf.len());
///         buf[..n].copy_from_slice(&self.0[..n]);
///         self.0 = &self.0[n..];
///         Poll::Ready(Ok(n))
///     }
/// }
/// ```
pub trait AsyncRead {
    /// Attempt to read bytes into `buf`. Returns the number of bytes
    /// written, or `Pending` if the underlying transport has nothing
    /// available yet.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    /// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
    /// # let (mut b, _) = listener.accept().await?;
    /// // `a` and `b` are the two ends of a TCP connection
    /// use core::future::poll_fn;
    /// use core::pin::Pin;
    /// use kevy_client_async::{AsyncRead, write_all};
    ///
    /// write_all(&mut a, b"hi").await?;
    /// let mut buf = [0u8; 8];
    /// let n = poll_fn(|cx| Pin::new(&mut b).poll_read(cx, &mut buf)).await?;
    /// assert_eq!(&buf[..n], b"hi");
    /// # Ok(()) }
    /// ```
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>>;
}

/// Async equivalent of [`std::io::Write`] — poll-based, owned-buffer.
///
/// The write half of a custom transport; see [`AsyncRead`].
///
/// # Contract
///
/// - `Poll::Pending` from any method means the transport cannot make
///   progress yet, and the waker in `cx` has been registered to fire when
///   it can.
/// - `poll_write` returning `Ready(Ok(n))` means the first `n` bytes of
///   `buf` were accepted, in order, and will reach the peer after any
///   bytes accepted before them; `Ready(Ok(0))` for a non-empty `buf`
///   means the transport can accept nothing more, and the client fails
///   the write with `WriteZero`.
/// - `poll_flush` resolves once every accepted byte has been handed to the
///   underlying medium.
/// - `poll_close` flushes and then shuts down the write half; nothing may
///   be written after it resolves.
///
/// ```
/// use core::pin::Pin;
/// use core::task::{Context, Poll};
/// use kevy_client_async::AsyncWrite;
///
/// /// A transport that records everything written to it.
/// struct Recorder(Vec<u8>);
///
/// impl AsyncWrite for Recorder {
///     fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
///         self.0.extend_from_slice(buf);
///         Poll::Ready(Ok(buf.len()))
///     }
///     fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
///         Poll::Ready(Ok(()))
///     }
///     fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
///         Poll::Ready(Ok(()))
///     }
/// }
/// ```
pub trait AsyncWrite {
    /// Attempt to write bytes from `buf`. Returns the number of bytes
    /// accepted, or `Pending`.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    /// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
    /// # let (mut b, _) = listener.accept().await?;
    /// // `a` and `b` are the two ends of a TCP connection
    /// use core::future::poll_fn;
    /// use core::pin::Pin;
    /// use kevy_client_async::{AsyncWrite, read};
    ///
    /// let n = poll_fn(|cx| Pin::new(&mut a).poll_write(cx, b"hi")).await?;
    /// assert_eq!(n, 2); // both bytes accepted
    /// let mut buf = [0u8; 8];
    /// let got = read(&mut b, &mut buf).await?;
    /// assert_eq!(&buf[..got], b"hi");
    /// # Ok(()) }
    /// ```
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>>;

    /// Attempt to flush buffered bytes to the transport.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    /// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
    /// # let (mut b, _) = listener.accept().await?;
    /// // `a` and `b` are the two ends of a TCP connection
    /// use core::future::poll_fn;
    /// use core::pin::Pin;
    /// use kevy_client_async::{AsyncWrite, write_all};
    ///
    /// write_all(&mut a, b"hi").await?;
    /// // every accepted byte is on its way once this resolves
    /// poll_fn(|cx| Pin::new(&mut a).poll_flush(cx)).await?;
    /// # Ok(()) }
    /// ```
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    /// Initiate / continue a graceful shutdown of the write half.
    ///
    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    /// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
    /// # let (mut b, _) = listener.accept().await?;
    /// // `a` and `b` are the two ends of a TCP connection
    /// use core::future::poll_fn;
    /// use core::pin::Pin;
    /// use kevy_client_async::{AsyncWrite, read};
    ///
    /// poll_fn(|cx| Pin::new(&mut a).poll_close(cx)).await?;
    /// // the peer sees end of stream
    /// let mut buf = [0u8; 8];
    /// assert_eq!(read(&mut b, &mut buf).await?, 0);
    /// # Ok(()) }
    /// ```
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

/// Bound used everywhere downstream: codec, `AsyncConnection`,
/// pipeline runner. Blanket-impl'd so any
/// `AsyncRead + AsyncWrite + Send + Unpin` value satisfies it.
///
/// Do not implement it directly: implement [`AsyncRead`] and
/// [`AsyncWrite`] (each under its contract) on a `Send + Unpin` type,
/// and the blanket impl makes it a transport.
///
/// ```
/// fn is_transport<T: kevy_client_async::AsyncTransport>() {}
/// # #[cfg(feature = "tokio")]
/// is_transport::<tokio::net::TcpStream>();
/// ```
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
///
/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> std::io::Result<()> {
/// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
/// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
/// # let (mut b, _) = listener.accept().await?;
/// // `a` and `b` are the two ends of a TCP connection
/// use kevy_client_async::{read, transport::Read, write_all};
///
/// write_all(&mut a, b"hi").await?;
/// let mut buf = [0u8; 8];
/// let pending: Read<'_, _> = read(&mut b, &mut buf); // nothing happens until awaited
/// assert_eq!(pending.await?, 2);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Read<'a, T: ?Sized> {
    transport: &'a mut T,
    buf: &'a mut [u8],
}

/// Future returned by [`write_all`]: drives `poll_write` to completion
/// for the whole buffer (loops on partial writes internally).
///
/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> std::io::Result<()> {
/// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
/// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
/// # let (mut b, _) = listener.accept().await?;
/// // `a` and `b` are the two ends of a TCP connection
/// use kevy_client_async::{read, transport::WriteAll, write_all};
///
/// let pending: WriteAll<'_, _> = write_all(&mut a, b"hi"); // nothing is sent until awaited
/// pending.await?;
/// let mut buf = [0u8; 8];
/// assert_eq!(read(&mut b, &mut buf).await?, 2);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct WriteAll<'a, T: ?Sized> {
    transport: &'a mut T,
    buf: &'a [u8],
    written: usize,
}

/// Single-chunk async read. Resolves to the number of bytes read; `0`
/// = clean EOF.
///
/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> std::io::Result<()> {
/// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
/// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
/// # let (mut b, _) = listener.accept().await?;
/// // `a` and `b` are the two ends of a TCP connection
/// use kevy_client_async::{read, write_all};
///
/// write_all(&mut a, b"hello").await?;
/// drop(a);
/// let (mut buf, mut got) = ([0u8; 8], Vec::new());
/// loop {
///     match read(&mut b, &mut buf).await? {
///         0 => break, // clean end of stream
///         n => got.extend_from_slice(&buf[..n]),
///     }
/// }
/// assert_eq!(got, b"hello");
/// # Ok(()) }
/// ```
pub fn read<'a, T>(transport: &'a mut T, buf: &'a mut [u8]) -> Read<'a, T>
where
    T: AsyncRead + Unpin + ?Sized,
{
    Read { transport, buf }
}

/// Async equivalent of `Write::write_all` — succeeds only after every
/// byte in `buf` is accepted by the transport.
///
/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> std::io::Result<()> {
/// # let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
/// # let mut a = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
/// # let (mut b, _) = listener.accept().await?;
/// // `a` and `b` are the two ends of a TCP connection
/// use kevy_client_async::{read, write_all};
///
/// let big = vec![7u8; 1 << 20]; // more than one socket write can take
/// let reader = tokio::spawn(async move {
///     let (mut buf, mut total) = ([0u8; 8192], 0);
///     loop {
///         match read(&mut b, &mut buf).await? {
///             0 => return Ok::<_, std::io::Error>(total),
///             n => total += n,
///         }
///     }
/// });
/// write_all(&mut a, &big).await?;
/// drop(a);
/// assert_eq!(reader.await.unwrap()?, big.len());
/// # Ok(()) }
/// ```
pub fn write_all<'a, T>(transport: &'a mut T, buf: &'a [u8]) -> WriteAll<'a, T>
where
    T: AsyncWrite + Unpin + ?Sized,
{
    WriteAll { transport, buf, written: 0 }
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
