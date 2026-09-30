//! Encrypted connections to a kevy server's `[secure] listen_port`: a
//! Noise IK handshake against the server's public key, then every byte
//! both ways sealed. [`SecureStream`] is a plain `Read + Write` stream, so
//! anything that speaks RESP over a socket can run over it.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use std::sync::{Arc, Mutex};

use kevy_noise::{Frames, Initiator, Keypair, MAX_MESSAGE, Opener, Sealer, frame};

/// Must match the server's.
const PROLOGUE: &[u8] = b"kevy-client\x001";
const TAG: usize = 16;

/// A TCP stream to a kevy encrypted client port, sealed both ways.
///
/// ```
/// use std::io::{Read, Write};
/// use kevy_resp_client::SecureStream;
/// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
/// # let (port, key) = doc::serve_secure();
///
/// let server_key = key; // printed by `kevy keygen` on the server
/// let mut s = SecureStream::connect("127.0.0.1", port, server_key, None)?;
/// s.write_all(b"*1\r\n$4\r\nPING\r\n")?;
/// let mut reply = [0u8; 7];
/// s.read_exact(&mut reply)?;
/// assert_eq!(&reply, b"+PONG\r\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct SecureStream {
    sock: TcpStream,
    /// Shared with any [`SecureWriter`]; held across sealing AND writing,
    /// so messages reach the wire in nonce order.
    tx: Arc<Mutex<Sealer>>,
    rx: Opener,
    frames: Frames,
    plain: Vec<u8>,
    pos: usize,
    chunk: Box<[u8]>,
}

impl std::fmt::Debug for SecureStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureStream").field("sock", &self.sock).finish_non_exhaustive()
    }
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Seal `buf` into as many Noise messages as it needs and write them in one
/// call, under the sealer's lock.
fn send_sealed(tx: &Mutex<Sealer>, sock: &mut TcpStream, mut buf: &[u8]) -> io::Result<()> {
    let mut tx = tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = Vec::with_capacity(buf.len() + 2 * (TAG + 2));
    while !buf.is_empty() {
        let n = buf.len().min(MAX_MESSAGE - TAG);
        out.extend(frame(&tx.seal(&buf[..n]).map_err(bad)?).map_err(bad)?);
        buf = &buf[n..];
    }
    sock.write_all(&out)
}

impl SecureStream {
    /// Connect and handshake. The server must hold the private half of
    /// `server_key`; `client` is this side's key pair, needed when the
    /// server lists `client_keys`. Without one, a fresh key pair is drawn
    /// for this connection: still encrypted, just not a listed identity.
    ///
    /// ```
    /// # use kevy_resp_client::SecureStream;
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
    /// # let (port, server_key) = doc::serve_secure();
    /// assert!(SecureStream::connect("127.0.0.1", port, server_key, None).is_ok());
    /// let refused = SecureStream::connect("127.0.0.1", port, [0; 32], None);
    /// assert!(refused.is_err()); // this server does not hold that key
    /// ```
    pub fn connect(
        host: &str,
        port: u16,
        server_key: [u8; 32],
        client: Option<&Keypair>,
    ) -> io::Result<Self> {
        let sock = TcpStream::connect((host, port))?;
        sock.set_nodelay(true)?;
        Self::handshake(sock, server_key, client)
    }

    /// [`Self::connect`] over a socket the caller has already opened, for
    /// callers that dial with their own timeout or address choice.
    ///
    /// ```
    /// # use kevy_resp_client::SecureStream;
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
    /// # let (port, server_key) = doc::serve_secure();
    /// let tcp = std::net::TcpStream::connect(("127.0.0.1", port))?;
    /// tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    /// let s = SecureStream::handshake(tcp, server_key, None)?;
    /// assert_eq!(s.socket().peer_addr()?.port(), port);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn handshake(
        mut sock: TcpStream,
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
        let ephemeral = Keypair::from_secret(random32()?);
        let (m1, init) =
            Initiator::start(local, &server_key, ephemeral, PROLOGUE, b"").map_err(bad)?;
        sock.write_all(&frame(&m1).map_err(bad)?)?;
        let mut frames = Frames::default();
        let mut chunk = vec![0u8; 64 * 1024].into_boxed_slice();
        let m2 = loop {
            if let Some(m) = frames.next() {
                break m;
            }
            let n = sock.read(&mut chunk)?;
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
        let tx = Arc::new(Mutex::new(tx));
        Ok(Self { sock, tx, rx, frames, plain: Vec::new(), pos: 0, chunk })
    }

    /// The underlying socket, for timeouts and shutdown.
    ///
    /// ```
    /// # use kevy_resp_client::SecureStream;
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
    /// # let (port, server_key) = doc::serve_secure();
    /// let s = SecureStream::connect("127.0.0.1", port, server_key, None)?;
    /// let timeout = Some(std::time::Duration::from_secs(1));
    /// s.socket().set_read_timeout(timeout)?;
    /// assert_eq!(s.socket().read_timeout()?, timeout);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn socket(&self) -> &TcpStream {
        &self.sock
    }

    /// Plaintext already decrypted and not yet read. A caller that waits on
    /// the socket before reading must read these first: they will not make
    /// the socket readable again.
    ///
    /// ```
    /// use std::io::{Read, Write};
    /// # use kevy_resp_client::SecureStream;
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
    /// # let (port, server_key) = doc::serve_secure();
    /// let mut s = SecureStream::connect("127.0.0.1", port, server_key, None)?;
    /// assert_eq!(s.buffered(), 0);
    /// s.write_all(b"*1\r\n$4\r\nPING\r\n")?;
    /// let mut first = [0u8; 1];
    /// s.read_exact(&mut first)?; // decrypts the whole `+PONG\r\n` message
    /// assert_eq!(s.buffered(), 6, "the rest of it waits here, not on the socket");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn buffered(&self) -> usize {
        self.plain.len() - self.pos
    }

    /// A second handle that writes into the same session, for another
    /// thread; messages from both are sealed in the order they reach the
    /// wire.
    ///
    /// ```
    /// # use std::io::{Read, Write};
    /// # use kevy_resp_client::SecureStream;
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
    /// # let (port, server_key) = doc::serve_secure();
    /// let mut s = SecureStream::connect("127.0.0.1", port, server_key, None)?;
    /// let mut w = s.writer()?;
    /// std::thread::spawn(move || w.write_all(b"*1\r\n$4\r\nPING\r\n")).join().unwrap()?;
    /// let mut reply = [0u8; 7];
    /// s.read_exact(&mut reply)?; // the reply comes back on the reading half
    /// assert_eq!(&reply, b"+PONG\r\n");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn writer(&self) -> io::Result<SecureWriter> {
        Ok(SecureWriter { sock: self.sock.try_clone()?, tx: Arc::clone(&self.tx) })
    }
}

impl Read for SecureStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.plain.len() {
            self.plain.clear();
            self.pos = 0;
            while let Some(m) = self.frames.next() {
                self.plain.extend(self.rx.open(&m).map_err(bad)?);
            }
            if !self.plain.is_empty() {
                break;
            }
            let n = self.sock.read(&mut self.chunk)?;
            if n == 0 {
                return Ok(0);
            }
            self.frames.push(&self.chunk[..n]);
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for SecureStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(MAX_MESSAGE - TAG);
        send_sealed(&self.tx, &mut self.sock, &buf[..n])?;
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        send_sealed(&self.tx, &mut self.sock, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// The writing half of a [`SecureStream`], from [`SecureStream::writer`].
///
/// ```
/// # use std::io::{Read, Write};
/// # use kevy_resp_client::SecureStream;
/// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs")); }
/// # let (port, server_key) = doc::serve_secure();
/// let mut s = SecureStream::connect("127.0.0.1", port, server_key, None)?;
/// let mut w: kevy_resp_client::SecureWriter = s.writer()?;
/// w.write_all(b"*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n")?;
/// let mut reply = [0u8; 8];
/// s.read_exact(&mut reply)?;
/// assert_eq!(&reply, b"$2\r\nhi\r\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct SecureWriter {
    sock: TcpStream,
    tx: Arc<Mutex<Sealer>>,
}

impl std::fmt::Debug for SecureWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureWriter").field("sock", &self.sock).finish_non_exhaustive()
    }
}

impl Write for SecureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(MAX_MESSAGE - TAG);
        send_sealed(&self.tx, &mut self.sock, &buf[..n])?;
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        send_sealed(&self.tx, &mut self.sock, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn random32() -> io::Result<[u8; 32]> {
    let mut b = [0u8; 32];
    kevy_sys::fill_random(&mut b)?;
    Ok(b)
}

#[cfg(target_arch = "wasm32")]
fn random32() -> io::Result<[u8; 32]> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientStream;
    use crate::{SecureUrl, load_client_key};
    use kevy_noise::Responder;
    use std::net::TcpListener;
    use std::path::Path;

    const SERVER: [u8; 32] = [1; 32];

    /// One-connection Noise echo server: returns every message sealed back,
    /// and closes the connection when a message reads `close`.
    fn echo_server() -> (u16, [u8; 32]) {
        let key = Keypair::from_secret(SERVER);
        let public = key.public();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut frames = Frames::default();
            let mut chunk = [0u8; 4096];
            let mut next = |s: &mut TcpStream, f: &mut Frames| loop {
                if let Some(m) = f.next() {
                    return Some(m);
                }
                let n = s.read(&mut chunk).ok()?;
                if n == 0 {
                    return None;
                }
                f.push(&chunk[..n]);
            };
            let m1 = next(&mut s, &mut frames).unwrap();
            let (_, r) =
                Responder::accept(&key, Keypair::from_secret([2; 32]), PROLOGUE, &m1).unwrap();
            let (m2, mut t) = r.finish(b"").unwrap();
            s.write_all(&frame(&m2).unwrap()).unwrap();
            while let Some(m) = next(&mut s, &mut frames) {
                let plain = t.open(&m).unwrap();
                if plain == b"close" {
                    return;
                }
                s.write_all(&frame(&t.seal(&plain).unwrap()).unwrap()).unwrap();
            }
        });
        (port, public)
    }

    fn read_n(r: &mut impl Read, n: usize) -> Vec<u8> {
        let mut out = vec![0u8; n];
        r.read_exact(&mut out).unwrap();
        out
    }

    #[test]
    fn a_secure_stream_writes_in_message_sized_pieces_and_sees_the_close() {
        let (port, key) = echo_server();
        let mut s = SecureStream::connect("127.0.0.1", port, key, None).unwrap();
        assert!(format!("{s:?}").starts_with("SecureStream"));
        let big: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
        let first = s.write(&big).unwrap();
        assert_eq!(first, MAX_MESSAGE - TAG, "one Noise message per write");
        s.write_all(&big[first..]).unwrap();
        s.flush().unwrap();
        assert_eq!(read_n(&mut s, big.len()), big);
        s.write_all(b"close").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(s.read(&mut buf).unwrap(), 0, "end of stream after the server closes");
    }

    #[test]
    fn a_client_stream_opens_kevys_with_a_key_file() {
        let (port, key) = echo_server();
        let dir = std::env::temp_dir().join(format!("kevy-resp-client-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("client.key");
        std::fs::write(&file, format!("{}\n", "cd".repeat(32))).unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let url =
            format!("kevys://127.0.0.1:{port}?server_key={hex}&client_key_file={}", file.display());
        let mut c = ClientStream::connect_url(&url).unwrap();
        assert!(matches!(c, ClientStream::Secure(_)));
        assert!(c.socket().peer_addr().is_ok());
        assert_eq!(c.write(b"ping").unwrap(), 4);
        c.flush().unwrap();
        assert_eq!(read_n(&mut c, 4), b"ping");
        std::fs::write(&file, "not a key").unwrap();
        assert!(ClientStream::connect_url(&url).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_kevys_urls_are_refused_before_connecting() {
        let k = "ab".repeat(32);
        for url in [
            "kevy://h:1".to_string(),
            "kevys://h:1".to_string(),
            "kevys://h:1?server_key=abc".to_string(),
            format!("kevys://h:1?server_key={k}&colour=blue"),
            format!("kevys://:1?server_key={k}"),
            format!("kevys://h:1?server_key={}", "zz".repeat(32)),
        ] {
            let e = SecureUrl::parse(&url).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{url}");
        }
        assert!(load_client_key(Path::new("/nonexistent/kevy.key")).is_err());
    }

    #[test]
    fn two_writers_share_one_session_in_wire_order() {
        let (port, key) = echo_server();
        let mut s = SecureStream::connect("127.0.0.1", port, key, None).unwrap();
        let mut w = s.writer().unwrap();
        assert!(format!("{w:?}").starts_with("SecureWriter"));
        let other = std::thread::spawn(move || {
            for _ in 0..500 {
                w.write_all(b"b").unwrap();
            }
            w.flush().unwrap();
            assert_eq!(w.write(b"b").unwrap(), 1);
        });
        for _ in 0..500 {
            s.write_all(b"a").unwrap();
        }
        other.join().unwrap();
        // the echo server opens every message in turn: one out of nonce
        // order would have closed the connection instead
        let back = read_n(&mut s, 1001);
        assert_eq!(back.iter().filter(|&&b| b == b'a').count(), 500);
        assert_eq!(back.iter().filter(|&&b| b == b'b').count(), 501);
    }

    #[test]
    fn buffered_counts_plaintext_decrypted_but_not_read() {
        let (port, key) = echo_server();
        let mut s = SecureStream::connect("127.0.0.1", port, key, None).unwrap();
        s.write_all(b"0123456789").unwrap();
        let mut one = [0u8; 1];
        s.read_exact(&mut one).unwrap();
        assert_eq!(s.buffered(), 9, "the rest of the message waits in the stream");
        assert_eq!(read_n(&mut s, 9), b"123456789");
        assert_eq!(s.buffered(), 0);
    }
}
