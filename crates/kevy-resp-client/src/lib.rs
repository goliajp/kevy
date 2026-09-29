//! Blocking RESP2 client over `TcpStream`.
//!
//! [`RespClient::connect`] opens a TCP connection (with `TCP_NODELAY`);
//! [`RespClient::request`] writes one command and blocks until exactly one
//! reply is parsed. Works against any RESP2 server — kevy, valkey, redis.
//!
//! [`RespClient::connect_url`] is the URL-string entry point and accepts
//! `kevy://` (kevy-native alias), `redis://` (standard), and `tcp://`
//! (plain host:port — no leading SELECT round-trip):
//!
//! ```no_run
//! # use kevy_resp_client::RespClient;
//! let _ = RespClient::connect_url("kevy://localhost:6379")?;   // alias of redis://
//! let _ = RespClient::connect_url("kevy://localhost:6379/0")?; // also issues SELECT 0
//! let _ = RespClient::connect_url("redis://10.0.0.5:6379")?;
//! let _ = RespClient::connect_url("tcp://kevy.internal:6379")?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Single-threaded; one client per thread. Holds an incremental read buffer
//! so multi-segment replies reassemble across `read` calls.
//!
//! Pure Rust, only deps are `std` + [`kevy_resp`].
//!
//! # Example
//!
//! ```no_run
//! use kevy_resp_client::RespClient;
//!
//! let mut c = RespClient::connect("127.0.0.1", 6379)?;
//! let reply = c.request(&[b"PING".to_vec()])?;
//! println!("{reply:?}");
//! # Ok::<(), std::io::Error>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use kevy_resp::Reply;
use kevy_resp::{encode_command, encode_command_borrowed};
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// A connection to a server's plaintext or encrypted client port; either
/// way a byte stream to run RESP over.
///
/// ```no_run
/// use std::io::Write;
/// use kevy_resp_client::ClientStream;
///
/// let mut s = ClientStream::connect_url("kevy://127.0.0.1:6379")?;
/// s.socket().set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
/// s.write_all(b"*1\r\n$4\r\nPING\r\n")?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientStream {
    /// The plaintext port.
    ///
    /// ```no_run
    /// let s = kevy_resp_client::ClientStream::Plain(std::net::TcpStream::connect("127.0.0.1:6379")?);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Plain(TcpStream),
    /// The encrypted port.
    ///
    /// ```no_run
    /// let s = kevy_resp_client::SecureStream::connect("127.0.0.1", 6404, [0xab; 32], None)?;
    /// let s = kevy_resp_client::ClientStream::Secure(Box::new(s));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Secure(Box<SecureStream>),
}

impl ClientStream {
    /// Connect by URL: `kevy://`, `redis://` and `tcp://` to the plaintext
    /// port, `kevys://` to the encrypted one. A `/db` path is not acted on
    /// here; [`RespClient::connect_url`] issues the `SELECT`.
    ///
    /// ```no_run
    /// let s = kevy_resp_client::ClientStream::connect_url(&format!(
    ///     "kevys://10.0.0.5:6404?server_key={}",
    ///     "ab".repeat(32)
    /// ))?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn connect_url(url: &str) -> io::Result<Self> {
        Ok(Self::open(url)?.0)
    }

    fn open(url: &str) -> io::Result<(Self, Option<u32>)> {
        if url.starts_with("kevys://") {
            let u = SecureUrl::parse(url)?;
            let me = u.client_key_file.as_deref().map(load_client_key).transpose()?;
            let s = SecureStream::connect(&u.host, u.port, u.server_key, me.as_ref())?;
            return Ok((Self::Secure(Box::new(s)), u.db));
        }
        let parsed = ParsedUrl::parse(url)?;
        let s = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
        s.set_nodelay(true).ok();
        Ok((Self::Plain(s), parsed.db))
    }

    /// The TCP socket underneath, for timeouts and shutdown.
    ///
    /// ```no_run
    /// let s = kevy_resp_client::ClientStream::connect_url("kevy://127.0.0.1:6379")?;
    /// println!("{}", s.socket().local_addr()?);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn socket(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            Self::Secure(s) => s.socket(),
        }
    }
}

impl Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ClientStream::Plain(s) => s.read(buf),
            ClientStream::Secure(s) => s.read(buf),
        }
    }
}

impl Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ClientStream::Plain(s) => s.write(buf),
            ClientStream::Secure(s) => s.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            ClientStream::Plain(s) => s.write_all(buf),
            ClientStream::Secure(s) => s.write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ClientStream::Plain(s) => s.flush(),
            ClientStream::Secure(s) => s.flush(),
        }
    }
}

/// A blocking RESP2 connection over `TcpStream`.
///
/// Holds the stream plus an incremental read buffer so multi-segment replies
/// reassemble across `read` calls. Not `Sync`; one client per thread.
///
/// ```
/// use kevy_resp_client::{Reply, RespClient};
/// # fn mock(replies: &'static [&'static [u8]]) -> u16 {
/// #     let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
/// #     let port = l.local_addr().unwrap().port();
/// #     std::thread::spawn(move || {
/// #         let (mut s, _) = l.accept().unwrap();
/// #         let mut pending = Vec::new();
/// #         for r in replies {
/// #             if !kevy_testnet::read_request(&mut s, &mut pending) { break }
/// #             std::io::Write::write_all(&mut s, r).unwrap();
/// #         }
/// #     });
/// #     port
/// # }
/// // a stand-in server that answers two commands
/// let port = mock(&[b"+OK\r\n", b"$5\r\nworld\r\n"]);
/// let mut c = RespClient::connect("127.0.0.1", port)?;
/// assert_eq!(c.request_borrowed(&[b"SET", b"hello", b"world"])?, Reply::Simple(b"OK".to_vec()));
/// assert_eq!(c.request_borrowed(&[b"GET", b"hello"])?, Reply::Bulk(b"world".to_vec()));
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct RespClient {
    stream: ClientStream,
    /// Incremental read buffer with a consume cursor — replies are parsed
    /// off the front by advancing a `pos` cursor rather than
    /// front-draining per reply (O(N²) on deep `pipeline_raw` batches).
    buf: ReplyReadBuf,
    /// Reused per-request encode buffer. Zero-allocation for steady-state
    /// command traffic — the buffer grows once during the first SET, then
    /// the same allocation backs every subsequent encode (truncated to 0
    /// at the top of each `request*` call). Added after profiling showed
    /// Rust-client `Vec<Vec<u8>>` argv + per-call `Vec<u8>::new()` was a
    /// measurable per-op tax even on a single connection.
    write_buf: Vec<u8>,
    /// Reused read scratch chunk — hoisted out of `read_one_reply` so the
    /// hot path doesn't zero-init an 8 KiB stack array on every call.
    chunk: Box<[u8]>,
}

impl RespClient {
    /// Connect to `host:port`, enabling `TCP_NODELAY` (best-effort).
    ///
    /// ```
    /// use kevy_resp_client::{Reply, RespClient};
    /// # fn mock(replies: &'static [&'static [u8]]) -> u16 {
    /// #     let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    /// #     let port = l.local_addr().unwrap().port();
    /// #     std::thread::spawn(move || {
    /// #         let (mut s, _) = l.accept().unwrap();
    /// #         let mut pending = Vec::new();
    /// #         for r in replies {
    /// #             if !kevy_testnet::read_request(&mut s, &mut pending) { break }
    /// #             std::io::Write::write_all(&mut s, r).unwrap();
    /// #         }
    /// #     });
    /// #     port
    /// # }
    /// let mut c = RespClient::connect("127.0.0.1", mock(&[b"+PONG\r\n"]))?;
    /// assert_eq!(c.request_borrowed(&[b"PING"])?, Reply::Simple(b"PONG".to_vec()));
    /// // nothing listens on port 1
    /// assert!(RespClient::connect("127.0.0.1", 1).is_err());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true).ok();
        Ok(Self::over(ClientStream::Plain(stream)))
    }

    /// Connect to a server's encrypted client port (`[secure] listen_port`).
    /// `server_key` is the server's public key; `client` is this side's
    /// key pair when the server lists `client_keys`, `None` otherwise.
    ///
    /// ```no_run
    /// use kevy_resp_client::RespClient;
    ///
    /// let mut c = RespClient::connect_secure("10.0.0.5", 6404, [0xab; 32], None)?;
    /// c.request_borrowed(&[b"PING"])?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn connect_secure(
        host: &str,
        port: u16,
        server_key: [u8; 32],
        client: Option<&Keypair>,
    ) -> io::Result<Self> {
        let s = SecureStream::connect(host, port, server_key, client)?;
        Ok(Self::over(ClientStream::Secure(Box::new(s))))
    }

    fn over(stream: ClientStream) -> Self {
        Self {
            stream,
            buf: ReplyReadBuf::with_capacity(8192),
            write_buf: Vec::with_capacity(1024),
            chunk: vec![0u8; 8192].into_boxed_slice(),
        }
    }

    /// Send one command (`args` is RESP-encoded as a multibulk array) and
    /// block until exactly one reply is parsed. Returns the parsed [`Reply`].
    ///
    /// **Prefer [`Self::request_borrowed`]** for new code: it takes
    /// `&[&[u8]]` (a stack-allocated slice array) and skips the per-call
    /// `Vec<Vec<u8>>` argv heap allocations. This `request` form remains
    /// for callers that already own `Vec<u8>` argvs.
    ///
    /// ```
    /// use kevy_resp_client::{Reply, RespClient};
    /// # fn mock(replies: &'static [&'static [u8]]) -> u16 {
    /// #     let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    /// #     let port = l.local_addr().unwrap().port();
    /// #     std::thread::spawn(move || {
    /// #         let (mut s, _) = l.accept().unwrap();
    /// #         let mut pending = Vec::new();
    /// #         for r in replies {
    /// #             if !kevy_testnet::read_request(&mut s, &mut pending) { break }
    /// #             std::io::Write::write_all(&mut s, r).unwrap();
    /// #         }
    /// #     });
    /// #     port
    /// # }
    /// let mut c = RespClient::connect("127.0.0.1", mock(&[b":3\r\n"]))?;
    /// let key = String::from("counter").into_bytes(); // an argv the caller already owns
    /// let reply = c.request(&[b"INCRBY".to_vec(), key, b"3".to_vec()])?;
    /// assert_eq!(reply, Reply::Int(3));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn request(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        self.write_buf.clear();
        encode_command(&mut self.write_buf, args);
        self.stream.write_all(&self.write_buf)?;
        self.read_one_reply()
    }

    /// Zero-allocation request: argv is `&[&[u8]]`, so a caller can pass
    /// `&[b"SET", key, value]` (a stack array of borrowed slices) and the
    /// only allocation is the one-time growth of `self.write_buf`. The
    /// hot path becomes `write_buf.clear() + encode + write_all + read`,
    /// no per-op heap traffic.
    ///
    /// ```
    /// use kevy_resp_client::{Reply, RespClient};
    /// # fn mock(replies: &'static [&'static [u8]]) -> u16 {
    /// #     let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    /// #     let port = l.local_addr().unwrap().port();
    /// #     std::thread::spawn(move || {
    /// #         let (mut s, _) = l.accept().unwrap();
    /// #         let mut pending = Vec::new();
    /// #         for r in replies {
    /// #             if !kevy_testnet::read_request(&mut s, &mut pending) { break }
    /// #             std::io::Write::write_all(&mut s, r).unwrap();
    /// #         }
    /// #     });
    /// #     port
    /// # }
    /// let mut c = RespClient::connect("127.0.0.1", mock(&[b"$1\r\nv\r\n", b"-ERR boom\r\n"]))?;
    /// let key: &[u8] = b"k";
    /// assert_eq!(c.request_borrowed(&[b"GET", key])?, Reply::Bulk(b"v".to_vec()));
    /// // a server-side error is a reply, not an `Err`
    /// assert_eq!(c.request_borrowed(&[b"BOOM"])?, Reply::Error(b"ERR boom".to_vec()));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn request_borrowed(&mut self, args: &[&[u8]]) -> io::Result<Reply> {
        self.write_buf.clear();
        encode_command_borrowed(&mut self.write_buf, args);
        self.stream.write_all(&self.write_buf)?;
        self.read_one_reply()
    }

    /// Pipelined batch: send every pre-encoded command in
    /// `raw` as one write, then read exactly `n` replies. The caller
    /// encodes with [`encode_command`]/[`encode_command_borrowed`]
    /// into one buffer (migration import path: 512-deep batches).
    ///
    /// ```
    /// use kevy_resp::encode_command_borrowed;
    /// use kevy_resp_client::{Reply, RespClient};
    /// # fn mock(replies: &'static [&'static [u8]]) -> u16 {
    /// #     let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    /// #     let port = l.local_addr().unwrap().port();
    /// #     std::thread::spawn(move || {
    /// #         let (mut s, _) = l.accept().unwrap();
    /// #         let mut pending = Vec::new();
    /// #         for r in replies {
    /// #             if !kevy_testnet::read_request(&mut s, &mut pending) { break }
    /// #             std::io::Write::write_all(&mut s, r).unwrap();
    /// #         }
    /// #     });
    /// #     port
    /// # }
    /// let mut c = RespClient::connect("127.0.0.1", mock(&[b":1\r\n", b":2\r\n"]))?;
    /// let mut raw = Vec::new();
    /// encode_command_borrowed(&mut raw, &[&b"INCR"[..], b"n"]);
    /// encode_command_borrowed(&mut raw, &[&b"INCR"[..], b"n"]);
    /// // one write, two replies read back in order
    /// assert_eq!(c.pipeline_raw(&raw, 2)?, vec![Reply::Int(1), Reply::Int(2)]);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn pipeline_raw(&mut self, raw: &[u8], n: usize) -> io::Result<Vec<Reply>> {
        self.stream.write_all(raw)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.read_one_reply()?);
        }
        Ok(out)
    }

    fn read_one_reply(&mut self) -> io::Result<Reply> {
        loop {
            match self.buf.parse_next() {
                Ok(Some(reply)) => return Ok(reply),
                Ok(None) => {}
                Err(_) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed reply"));
                }
            }
            let n = self.stream.read(&mut self.chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                ));
            }
            self.buf.extend(&self.chunk[..n]);
        }
    }

    /// Connect from a URL string.
    ///
    /// Accepted schemes (all wire-protocol identical — RESP2 over TCP):
    /// - `kevy://host[:port][/db]` — kevy-native alias of `redis://`.
    /// - `redis://host[:port][/db]` — standard Redis URL (every official
    ///   client lib speaks this).
    /// - `tcp://host[:port]` — plain TCP with no leading SELECT round-trip.
    ///
    /// Auth and TLS schemes (`redis://user:pass@…`, `rediss://`) are NOT
    /// supported — kevy itself ships without AUTH/TLS. Including a userinfo
    /// component or using `rediss://` returns [`io::ErrorKind::Unsupported`].
    ///
    /// If a `/db` path segment is present, an explicit `SELECT <db>` is
    /// issued before returning the client. For non-zero indices kevy will
    /// reply with its "only supports DB 0" error and `connect_url`
    /// propagates that as [`io::ErrorKind::Other`].
    ///
    /// `kevys://host:port?server_key=<hex>[&client_key_file=<path>]`
    /// connects to the encrypted client port; see [`SecureUrl::parse`].
    ///
    /// ```
    /// use kevy_resp_client::{Reply, RespClient};
    /// # fn mock(replies: &'static [&'static [u8]]) -> u16 {
    /// #     let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    /// #     let port = l.local_addr().unwrap().port();
    /// #     std::thread::spawn(move || {
    /// #         let (mut s, _) = l.accept().unwrap();
    /// #         let mut pending = Vec::new();
    /// #         for r in replies {
    /// #             if !kevy_testnet::read_request(&mut s, &mut pending) { break }
    /// #             std::io::Write::write_all(&mut s, r).unwrap();
    /// #         }
    /// #     });
    /// #     port
    /// # }
    /// // `/0` makes the client send `SELECT 0` before handing it back
    /// let port = mock(&[b"+OK\r\n", b"+PONG\r\n"]);
    /// let mut c = RespClient::connect_url(&format!("kevy://127.0.0.1:{port}/0"))?;
    /// assert_eq!(c.request_borrowed(&[b"PING"])?, Reply::Simple(b"PONG".to_vec()));
    /// let e = RespClient::connect_url("rediss://127.0.0.1:6379").unwrap_err();
    /// assert_eq!(e.kind(), std::io::ErrorKind::Unsupported); // no TLS
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn connect_url(url: &str) -> io::Result<Self> {
        let (stream, db) = ClientStream::open(url)?;
        let mut client = Self::over(stream);
        if let Some(db) = db {
            let reply = client.request(&[b"SELECT".to_vec(), db.to_string().into_bytes()])?;
            if let Reply::Error(msg) = reply {
                let text = String::from_utf8_lossy(&msg);
                return Err(io::Error::other(format!("SELECT {db} rejected: {text}")));
            }
        }
        Ok(client)
    }
}

mod url;
pub use url::ParsedUrl;

mod secure;
pub use kevy_noise::Keypair;
pub use secure::{SecureStream, SecureWriter};
mod secure_url;
pub use secure_url::{SecureUrl, load_client_key};

pub use kevy_resp::PubsubEvent;

mod read_buf;
pub use read_buf::ReplyReadBuf;

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<ClientStream>();
    send_sync::<RespClient>();
    send_sync::<ReplyReadBuf>();
    send_sync::<PubsubEvent>();
    send_sync::<SecureUrl>();
    send_sync::<SecureStream>();
    send_sync::<SecureWriter>();
    send_sync::<ParsedUrl>();
};
