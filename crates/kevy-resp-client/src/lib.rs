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

/// A blocking RESP2 connection over `TcpStream`.
///
/// Holds the stream plus an incremental read buffer so multi-segment replies
/// reassemble across `read` calls. Not `Sync`; one client per thread.
pub struct RespClient {
    stream: TcpStream,
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
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            buf: ReplyReadBuf::with_capacity(8192),
            write_buf: Vec::with_capacity(1024),
            chunk: vec![0u8; 8192].into_boxed_slice(),
        })
    }

    /// Send one command (`args` is RESP-encoded as a multibulk array) and
    /// block until exactly one reply is parsed. Returns the parsed [`Reply`].
    ///
    /// **Prefer [`Self::request_borrowed`]** for new code: it takes
    /// `&[&[u8]]` (a stack-allocated slice array) and skips the per-call
    /// `Vec<Vec<u8>>` argv heap allocations. This `request` form remains
    /// for callers that already own `Vec<u8>` argvs.
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
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed reply",
                    ));
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
    pub fn connect_url(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let mut client = Self::connect(&parsed.host, parsed.port)?;
        if let Some(db) = parsed.db {
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
pub use url::{ParsedUrl, parse_url};

mod pubsub_event;
pub use pubsub_event::{PubsubEvent, classify_pubsub};

mod read_buf;
pub use read_buf::ReplyReadBuf;
