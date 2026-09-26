//! Encrypted connections to a kevy server's `[secure] listen_port`: a
//! Noise IK handshake against the server's public key, then every byte
//! both ways sealed. [`SecureStream`] is a plain `Read + Write` stream, so
//! anything that speaks RESP over a socket can run over it.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use kevy_noise::{Frames, Initiator, Keypair, MAX_MESSAGE, Transport, frame};

/// Must match the server's.
const PROLOGUE: &[u8] = b"kevy-client\x001";
const TAG: usize = 16;

/// A TCP stream to a kevy encrypted client port, sealed both ways.
///
/// ```no_run
/// use std::io::{Read, Write};
/// use kevy_resp_client::SecureStream;
///
/// let server_key = [0xab; 32]; // printed by `kevy keygen` on the server
/// let mut s = SecureStream::connect("127.0.0.1", 6404, server_key, None)?;
/// s.write_all(b"*1\r\n$4\r\nPING\r\n")?;
/// let mut reply = [0u8; 7];
/// s.read_exact(&mut reply)?;
/// assert_eq!(&reply, b"+PONG\r\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct SecureStream {
    sock: TcpStream,
    transport: Transport,
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

impl SecureStream {
    /// Connect and handshake. The server must hold the private half of
    /// `server_key`; `client` is this side's key pair, needed when the
    /// server lists `client_keys`. Without one, a fresh key pair is drawn
    /// for this connection: still encrypted, just not a listed identity.
    ///
    /// ```no_run
    /// # use kevy_resp_client::SecureStream;
    /// let refused = SecureStream::connect("127.0.0.1", 6404, [0; 32], None);
    /// assert!(refused.is_err()); // no server holds that key
    /// ```
    pub fn connect(
        host: &str,
        port: u16,
        server_key: [u8; 32],
        client: Option<&Keypair>,
    ) -> io::Result<Self> {
        let mut sock = TcpStream::connect((host, port))?;
        sock.set_nodelay(true)?;
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
        Ok(Self { sock, transport, frames, plain: Vec::new(), pos: 0, chunk })
    }

    /// The underlying socket, for timeouts and shutdown.
    ///
    /// ```no_run
    /// # use kevy_resp_client::SecureStream;
    /// let s = SecureStream::connect("127.0.0.1", 6404, [0xab; 32], None)?;
    /// s.socket().set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn socket(&self) -> &TcpStream {
        &self.sock
    }
}

impl Read for SecureStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.plain.len() {
            self.plain.clear();
            self.pos = 0;
            while let Some(m) = self.frames.next() {
                self.plain.extend(self.transport.open(&m).map_err(bad)?);
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
        let sealed = self.transport.seal(&buf[..n]).map_err(bad)?;
        self.sock.write_all(&frame(&sealed).map_err(bad)?)?;
        Ok(n)
    }

    fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        // one framed write per call rather than one per Noise message
        let mut out = Vec::with_capacity(buf.len() + 2 * (TAG + 2));
        while !buf.is_empty() {
            let n = buf.len().min(MAX_MESSAGE - TAG);
            out.extend(frame(&self.transport.seal(&buf[..n]).map_err(bad)?).map_err(bad)?);
            buf = &buf[n..];
        }
        self.sock.write_all(&out)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// The key pieces of a `kevys://` URL: the server's public key, and the
/// file holding this client's key pair, if any.
///
/// ```
/// let u = kevy_resp_client::parse_secure_url(&format!("kevys://h:6404?server_key={}", "ab".repeat(32)))?;
/// assert_eq!((u.host.as_str(), u.port, u.server_key), ("h", 6404, [0xab; 32]));
/// assert_eq!(u.client_key_file, None);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecureUrl {
    /// Hostname or IP literal.
    ///
    /// ```
    /// let u = kevy_resp_client::parse_secure_url(&format!("kevys://db.internal?server_key={}", "ab".repeat(32)))?;
    /// assert_eq!(u.host, "db.internal");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub host: String,
    /// TCP port of the encrypted client port; 6379 when omitted.
    ///
    /// ```
    /// let u = kevy_resp_client::parse_secure_url(&format!("kevys://h?server_key={}", "ab".repeat(32)))?;
    /// assert_eq!(u.port, 6379);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub port: u16,
    /// Optional db index from a `/N` path component.
    ///
    /// ```
    /// let u = kevy_resp_client::parse_secure_url(&format!("kevys://h:1/0?server_key={}", "ab".repeat(32)))?;
    /// assert_eq!(u.db, Some(0));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub db: Option<u32>,
    /// The server's public key, from `server_key=` (64 hex characters).
    ///
    /// ```
    /// assert!(kevy_resp_client::parse_secure_url("kevys://h:1").is_err()); // required
    /// ```
    pub server_key: [u8; 32],
    /// This client's private key file, from `client_key_file=`, in the
    /// format `kevy keygen` writes.
    ///
    /// ```
    /// let u = kevy_resp_client::parse_secure_url(&format!(
    ///     "kevys://h:1?server_key={}&client_key_file=/etc/app/kevy.key",
    ///     "ab".repeat(32)
    /// ))?;
    /// assert_eq!(u.client_key_file.as_deref(), Some(std::path::Path::new("/etc/app/kevy.key")));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub client_key_file: Option<PathBuf>,
}

/// Parse `kevys://host[:port][/db]?server_key=<hex>[&client_key_file=<path>]`.
///
/// ```
/// use kevy_resp_client::parse_secure_url;
/// assert!(parse_secure_url("kevy://h:1").is_err()); // not a kevys:// URL
/// assert!(parse_secure_url("kevys://h:1?server_key=abc").is_err()); // short key
/// assert!(parse_secure_url(&format!("kevys://h:1?server_key={}&x=1", "ab".repeat(32))).is_err());
/// ```
pub fn parse_secure_url(url: &str) -> io::Result<SecureUrl> {
    let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
    let rest = url
        .strip_prefix("kevys://")
        .ok_or_else(|| invalid(format!("not a kevys:// URL: {url}")))?;
    let (base, query) = rest.split_once('?').unwrap_or((rest, ""));
    let plain = crate::parse_url(&format!("kevy://{base}"))?;
    let (mut server_key, mut client_key_file) = (None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("server_key", v)) => server_key = Some(key_from_hex(v)?),
            Some(("client_key_file", v)) if !v.is_empty() => {
                client_key_file = Some(PathBuf::from(v))
            }
            _ => return Err(invalid(format!("unknown kevys:// parameter: {pair}"))),
        }
    }
    let server_key = server_key.ok_or_else(|| {
        invalid("kevys:// needs server_key=<the server's public key>".to_string())
    })?;
    Ok(SecureUrl { host: plain.host, port: plain.port, db: plain.db, server_key, client_key_file })
}

/// Read a key pair from a file holding the private key as 64 hex
/// characters, as `kevy keygen` writes it.
///
/// ```no_run
/// let me = kevy_resp_client::load_client_key(std::path::Path::new("/etc/app/kevy.key"))?;
/// println!("{:02x?}", me.public());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn load_client_key(path: &Path) -> io::Result<Keypair> {
    let text = std::fs::read_to_string(path)?;
    Ok(Keypair::from_secret(key_from_hex(&text)?))
}

fn key_from_hex(s: &str) -> io::Result<[u8; 32]> {
    let s = s.trim();
    let mut k = [0u8; 32];
    let ok = s.len() == 64
        && s.is_ascii()
        && k.iter_mut()
            .enumerate()
            .all(|(i, b)| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map(|v| *b = v).is_ok());
    if ok {
        Ok(k)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "a key is 64 hex characters"))
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
    use kevy_noise::Responder;
    use std::net::TcpListener;

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
            let e = parse_secure_url(&url).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{url}");
        }
        assert!(load_client_key(Path::new("/nonexistent/kevy.key")).is_err());
    }
}
