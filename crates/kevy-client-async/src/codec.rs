//! Async RESP3 codec — state machine mirroring the blocking
//! [`kevy_resp_client::RespClient`] but with async IO.
//!
//! Reuses `kevy_resp::{encode_command, parse_reply}` for the pure parse
//! / encode logic so the wire format never has two implementations.
//! What's different here is the IO loop: instead of blocking
//! `Read::read` we drive the codec on top of any
//! [`AsyncTransport`][crate::AsyncTransport].
//!
//! The state machine is identical to blocking:
//!
//! 1. `encode_command(args)` → `write_all` over the transport.
//! 2. Loop: `parse_reply(&buf)`; on `None` await one `read` chunk and
//!    extend `buf`; on `Some((reply, used))` drain `used` and return.
//!
//! No buffered allocations beyond what blocking does (one growable
//! `buf` for partial replies + one boxed chunk for reads). Pipelining
//! reuses this same codec — `run_pipeline` writes N commands in one
//! batch and reads N replies in sequence (T4.16).

use std::io;

use kevy_resp::{Reply, encode_command, parse_reply};

use crate::transport::{AsyncTransport, read, write_all};

/// Buffered RESP3 codec over an [`AsyncTransport`].
pub struct AsyncRespCodec<T> {
    transport: T,
    buf: Vec<u8>,
    chunk: Box<[u8]>,
}

impl<T: AsyncTransport> AsyncRespCodec<T> {
    /// Wrap a transport. Matches the blocking client's 8 KiB initial
    /// buffer capacity + 8 KiB read chunk — same memory footprint per
    /// connection as `RespClient`.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            buf: Vec::with_capacity(8192),
            chunk: vec![0u8; 8192].into_boxed_slice(),
        }
    }

    /// Get the underlying transport back (e.g. to swap it or close it
    /// explicitly).
    pub fn into_inner(self) -> T {
        self.transport
    }

    /// Send one command (`args` = multibulk argv) and await exactly one
    /// reply. Direct async mirror of `RespClient::request`.
    pub async fn request(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        self.send(args).await?;
        self.read_reply().await
    }

    /// Encode + write a single command without waiting for a reply.
    /// Used by [`crate::AsyncSubscriber`]: SUBSCRIBE / PSUBSCRIBE etc.
    /// don't return replies in the conventional sense — the server
    /// pushes ack frames that are drained later by `read_reply`.
    pub async fn send(&mut self, args: &[Vec<u8>]) -> io::Result<()> {
        let mut out = Vec::new();
        encode_command(&mut out, args);
        write_all(&mut self.transport, &out).await?;
        Ok(())
    }

    /// Drain one parsed reply from the read buffer, reading more bytes
    /// from the transport as needed. Pipelining (T4.16) calls this N
    /// times after a single batched write.
    pub async fn read_reply(&mut self) -> io::Result<Reply> {
        // Destructure so the loop can borrow `transport` and `chunk`
        // disjointly from `buf`.
        let Self { transport, buf, chunk } = self;
        loop {
            match parse_reply(buf) {
                Ok(Some((reply, used))) => {
                    buf.drain(..used);
                    return Ok(reply);
                }
                Ok(None) => {}
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed reply",
                    ));
                }
            }
            let n = read(transport, &mut chunk[..]).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                ));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Send N commands as one write batch (pipelining), then read N
    /// replies in declaration order. Single network round-trip if the
    /// transport supports it.
    pub async fn pipeline(&mut self, batch: &[Vec<Vec<u8>>]) -> io::Result<Vec<Reply>> {
        let mut out = Vec::new();
        for args in batch {
            encode_command(&mut out, args);
        }
        write_all(&mut self.transport, &out).await?;

        let mut replies = Vec::with_capacity(batch.len());
        for _ in batch {
            replies.push(self.read_reply().await?);
        }
        Ok(replies)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────
//
// Mock transport + minimal executor (no runtime dep) to verify
// round-trip without pulling tokio etc. into the test surface.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{AsyncRead, AsyncWrite};
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};

    /// In-memory transport: pre-loaded `incoming` bytes feed `poll_read`;
    /// `poll_write` appends to `outgoing`.
    struct MockTransport {
        incoming: Vec<u8>,
        in_cursor: usize,
        outgoing: Vec<u8>,
    }

    impl MockTransport {
        fn new(server_reply: Vec<u8>) -> Self {
            Self {
                incoming: server_reply,
                in_cursor: 0,
                outgoing: Vec::new(),
            }
        }
    }

    impl AsyncRead for MockTransport {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let remaining = self.incoming.len() - self.in_cursor;
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&self.incoming[self.in_cursor..self.in_cursor + n]);
            self.in_cursor += n;
            Poll::Ready(Ok(n))
        }
    }

    impl AsyncWrite for MockTransport {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.outgoing.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Minimal blocking executor — polls once; works for futures that
    /// never park (our mock returns `Poll::Ready` synchronously). Uses
    /// `Box::pin` so the test stays safe-only under `forbid(unsafe_code)`.
    fn block_on<F: Future>(fut: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut pinned = Box::pin(fut);
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("mock transport must never park"),
        }
    }

    #[test]
    fn request_sends_command_and_parses_reply() {
        // Server response: +OK\r\n (simple string)
        let mock = MockTransport::new(b"+OK\r\n".to_vec());
        let mut codec = AsyncRespCodec::new(mock);
        let reply = block_on(codec.request(&[b"PING".to_vec()])).unwrap();
        match reply {
            Reply::Simple(s) => assert_eq!(s, b"OK"),
            other => panic!("expected Simple, got {other:?}"),
        }
        // Outgoing wire is RESP multibulk [PING].
        let mock = codec.into_inner();
        assert_eq!(mock.outgoing, b"*1\r\n$4\r\nPING\r\n");
    }

    #[test]
    fn pipeline_batches_three_commands() {
        // Three replies concatenated: +A\r\n +B\r\n +C\r\n
        let mock = MockTransport::new(b"+A\r\n+B\r\n+C\r\n".to_vec());
        let mut codec = AsyncRespCodec::new(mock);
        let batch = vec![
            vec![b"PING".to_vec()],
            vec![b"PING".to_vec()],
            vec![b"PING".to_vec()],
        ];
        let replies = block_on(codec.pipeline(&batch)).unwrap();
        assert_eq!(replies.len(), 3);
        let mock = codec.into_inner();
        // Single write contained all three encoded commands.
        let expected: Vec<u8> = b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n"
            .to_vec();
        assert_eq!(mock.outgoing, expected);
    }
}
