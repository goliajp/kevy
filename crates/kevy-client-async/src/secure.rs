//! Encrypted connections to a kevy server's `[secure] listen_port`, over
//! any [`AsyncTransport`]: a Noise IK handshake against the server's public
//! key, then every byte both ways sealed.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

use kevy_noise::{Frames, Initiator, Keypair, MAX_MESSAGE, Opener, Sealer, frame};

use crate::transport::{AsyncRead, AsyncTransport, AsyncWrite, read, write_all};

/// Must match the server's.
const PROLOGUE: &[u8] = b"kevy-client\x001";
const TAG: usize = 16;

/// A transport wrapped in Noise: reads open, writes seal.
///
/// ```
/// # include!("doc_serve.rs");
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> std::io::Result<()> {
/// # let (addr, server_key) = serve_secure(&[("PING", "+PONG\r\n")]).await?;
/// use kevy_client_async::{AsyncRespCodec, AsyncSecure, rt_tokio};
/// use kevy_resp::Reply;
///
/// let tcp = rt_tokio::connect("127.0.0.1", addr.port()).await?;
/// let secure = AsyncSecure::handshake(tcp, server_key, None).await?;
/// let mut codec = AsyncRespCodec::new(secure);
/// assert_eq!(codec.request(&[b"PING".to_vec()]).await?, Reply::Simple(b"PONG".to_vec()));
/// # Ok(()) }
/// ```
pub struct AsyncSecure<T> {
    inner: T,
    tx: Sealer,
    rx: Opener,
    frames: Frames,
    plain: Vec<u8>,
    pos: usize,
    /// Sealed bytes not yet written, and how many plaintext bytes they carry.
    out: Vec<u8>,
    out_off: usize,
    accepted: usize,
    chunk: Box<[u8]>,
}

impl<T> std::fmt::Debug for AsyncSecure<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncSecure").field("unwritten", &(self.out.len() - self.out_off)).finish()
    }
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl<T: AsyncTransport> AsyncSecure<T> {
    /// Handshake over `inner`. The server must hold the private half of
    /// `server_key`; `client` is this side's key pair when the server lists
    /// `client_keys`, `None` for a fresh one per connection.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let (addr, server_key) = serve_secure(&[]).await?;
    /// use kevy_client_async::{AsyncSecure, rt_tokio};
    ///
    /// let tcp = rt_tokio::connect("127.0.0.1", addr.port()).await?;
    /// AsyncSecure::handshake(tcp, server_key, None).await?;
    ///
    /// // a server without the private half of the key is refused
    /// let tcp = rt_tokio::connect("127.0.0.1", addr.port()).await?;
    /// let wrong = AsyncSecure::handshake(tcp, [0; 32], None).await;
    /// assert!(wrong.is_err());
    /// # Ok(()) }
    /// ```
    pub async fn handshake(
        mut inner: T,
        server_key: [u8; 32],
        client: Option<&Keypair>,
    ) -> io::Result<Self> {
        let fresh;
        let local = match client {
            Some(k) => k,
            None => {
                fresh = Keypair::from_secret(random32()?);
                &fresh
            }
        };
        let (m1, init) =
            Initiator::start(local, &server_key, Keypair::from_secret(random32()?), PROLOGUE, b"")
                .map_err(bad)?;
        write_all(&mut inner, &frame(&m1).map_err(bad)?).await?;
        let mut frames = Frames::default();
        let mut chunk = vec![0u8; 64 * 1024].into_boxed_slice();
        let m2 = loop {
            if let Some(m) = frames.next() {
                break m;
            }
            let n = read(&mut inner, &mut chunk).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the server closed the handshake: wrong server key, or this client's key is not listed",
                ));
            }
            frames.push(&chunk[..n]);
        };
        let (_, transport) = init.finish(&m2).map_err(bad)?;
        let (tx, rx) = transport.split();
        Ok(Self {
            inner,
            tx,
            rx,
            frames,
            plain: Vec::new(),
            pos: 0,
            out: Vec::new(),
            out_off: 0,
            accepted: 0,
            chunk,
        })
    }

    /// Write out whatever sealed bytes are still pending.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.out_off < self.out.len() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_off..]) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(n)) => self.out_off += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.out.clear();
        self.out_off = 0;
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncTransport> AsyncRead for AsyncSecure<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        while this.pos == this.plain.len() {
            this.plain.clear();
            this.pos = 0;
            while let Some(m) = this.frames.next() {
                match this.rx.open(&m) {
                    Ok(p) => this.plain.extend(p),
                    Err(e) => return Poll::Ready(Err(bad(e))),
                }
            }
            if !this.plain.is_empty() {
                break;
            }
            let n = match Pin::new(&mut this.inner).poll_read(cx, &mut this.chunk) {
                Poll::Ready(Ok(n)) => n,
                other => return other,
            };
            if n == 0 {
                return Poll::Ready(Ok(0));
            }
            this.frames.push(&this.chunk[..n]);
        }
        let n = buf.len().min(this.plain.len() - this.pos);
        buf[..n].copy_from_slice(&this.plain[this.pos..this.pos + n]);
        this.pos += n;
        Poll::Ready(Ok(n))
    }
}

impl<T: AsyncTransport> AsyncWrite for AsyncSecure<T> {
    /// Seals up to one Noise message of `buf` and reports it written only
    /// once the ciphertext is on the transport. A caller retrying after
    /// `Pending` passes the same bytes again, and they are not sealed twice.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.out.is_empty() {
            let n = buf.len().min(MAX_MESSAGE - TAG);
            let sealed = match this.tx.seal(&buf[..n]).and_then(|s| frame(&s)) {
                Ok(f) => f,
                Err(e) => return Poll::Ready(Err(bad(e))),
            };
            this.out = sealed;
            this.accepted = n;
        }
        match this.poll_drain(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(this.accepted)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            other => other,
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_close(cx),
            other => other,
        }
    }
}

fn random32() -> io::Result<[u8; 32]> {
    let mut b = [0u8; 32];
    kevy_sys::fill_random(&mut b)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::task::Waker;

    /// An in-memory transport whose writes go into a shared vec and whose
    /// reads come from a scripted vec, returning `Pending` every other
    /// write to exercise the retry path.
    struct Mem {
        wrote: Arc<Mutex<Vec<u8>>>,
        to_read: Vec<u8>,
        flip: bool,
    }

    impl AsyncRead for Mem {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let n = buf.len().min(this.to_read.len()).min(7);
            buf[..n].copy_from_slice(&this.to_read[..n]);
            this.to_read.drain(..n);
            Poll::Ready(Ok(n))
        }
    }

    impl AsyncWrite for Mem {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.flip = !this.flip;
            if this.flip {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let n = buf.len().min(5);
            this.wrote.lock().unwrap().extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        let mut cx = Context::from_waker(Waker::noop());
        let mut f = std::pin::pin!(f);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    /// An established client/server pair, the client half wrapped around
    /// `inner`.
    fn pair(inner: Mem) -> (AsyncSecure<Mem>, Sealer, Opener) {
        let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
        let (m1, init) = Initiator::start(
            &client,
            &server.public(),
            Keypair::from_secret([3; 32]),
            PROLOGUE,
            b"",
        )
        .unwrap();
        let (_, r) =
            kevy_noise::Responder::accept(&server, Keypair::from_secret([4; 32]), PROLOGUE, &m1)
                .unwrap();
        let (m2, st) = r.finish(b"").unwrap();
        let (_, ct) = init.finish(&m2).unwrap();
        let ((tx, rx), (s_tx, s_rx)) = (ct.split(), st.split());
        let conn = AsyncSecure {
            inner,
            tx,
            rx,
            frames: Frames::default(),
            plain: Vec::new(),
            pos: 0,
            out: Vec::new(),
            out_off: 0,
            accepted: 0,
            chunk: vec![0u8; 16].into_boxed_slice(),
        };
        (conn, s_tx, s_rx)
    }

    #[test]
    fn a_write_retried_after_pending_is_sealed_once_and_reads_reassemble() {
        let wrote = Arc::new(Mutex::new(Vec::new()));
        let mem = Mem { wrote: Arc::clone(&wrote), to_read: Vec::new(), flip: false };
        let (mut conn, mut s_tx, mut s_rx) = pair(mem);
        let big: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
        block_on(write_all(&mut conn, &big)).unwrap();
        let mut f = Frames::default();
        f.push(&wrote.lock().unwrap());
        let mut got = Vec::new();
        while let Some(m) = f.next() {
            got.extend(s_rx.open(&m).unwrap());
        }
        assert_eq!(got, big, "every byte sealed exactly once, in order");

        conn.inner.to_read = frame(&s_tx.seal(b"+PONG\r\n").unwrap()).unwrap();
        let mut back = [0u8; 7];
        let mut n = 0;
        while n < back.len() {
            n += block_on(read(&mut conn, &mut back[n..])).unwrap();
        }
        assert_eq!(&back, b"+PONG\r\n");
        assert_eq!(block_on(read(&mut conn, &mut back)).unwrap(), 0, "end of stream");
        assert!(format!("{conn:?}").contains("unwritten: 0"));
    }

    #[test]
    fn flush_and_close_write_out_what_is_still_sealed() {
        let wrote = Arc::new(Mutex::new(Vec::new()));
        let mem = Mem { wrote: Arc::clone(&wrote), to_read: Vec::new(), flip: false };
        let (mut conn, _, mut s_rx) = pair(mem);
        let mut cx = Context::from_waker(Waker::noop());
        conn.out = frame(&conn.tx.seal(b"queued").unwrap()).unwrap();
        while Pin::new(&mut conn).poll_flush(&mut cx).is_pending() {}
        assert!(conn.out.is_empty());
        conn.out = frame(&conn.tx.seal(b"last").unwrap()).unwrap();
        while Pin::new(&mut conn).poll_close(&mut cx).is_pending() {}
        assert!(conn.out.is_empty(), "nothing left behind at close");
        let mut f = Frames::default();
        f.push(&wrote.lock().unwrap());
        assert_eq!(s_rx.open(&f.next().unwrap()).unwrap(), b"queued");
        assert_eq!(s_rx.open(&f.next().unwrap()).unwrap(), b"last");
    }
}
