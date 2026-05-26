//! Programmatic RESP client — the library half of `kevy-cli`.
//!
//! [`RespClient`] is a blocking, single-threaded RESP2 client over `TcpStream`.
//! Connect with [`RespClient::connect`]; send a command + read one reply with
//! [`RespClient::request`]. The `kevy-cli` binary is one consumer; integration
//! tests / scripts / glue tools can use the same type without re-implementing
//! the line/buffer protocol.
//!
//! Works against any RESP2 server (kevy, valkey, redis). Pure Rust, only deps
//! are `std` + [`kevy-resp`](https://crates.io/crates/kevy-resp).
//!
//! # Example
//!
//! ```no_run
//! use kevy_cli::RespClient;
//!
//! let mut c = RespClient::connect("127.0.0.1", 6379)?;
//! let reply = c.request(&[b"PING".to_vec()])?;
//! println!("{:?}", reply);
//! # Ok::<(), std::io::Error>(())
//! ```

#![forbid(unsafe_code)]

pub use kevy_resp::Reply;
use kevy_resp::{encode_command, parse_reply};
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// A blocking RESP2 connection.
///
/// Holds the TCP stream + an incremental read buffer so multi-segment replies
/// reassemble across `read` calls. Not `Sync`-safe; one client per thread.
pub struct RespClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl RespClient {
    /// Connect to `host:port`, enabling `TCP_NODELAY` (best-effort).
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            buf: Vec::with_capacity(8192),
        })
    }

    /// Send one command (`args` is RESP-encoded as a multibulk array) and
    /// block until exactly one reply is parsed. Returns the parsed [`Reply`].
    pub fn request(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        let mut out = Vec::new();
        encode_command(&mut out, args);
        self.stream.write_all(&out)?;

        let mut chunk = [0u8; 8192];
        loop {
            match parse_reply(&self.buf) {
                Ok(Some((reply, used))) => {
                    self.buf.drain(..used);
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
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Pretty-print a reply roughly the way `redis-cli` does. Arrays are
/// numbered + indented; bulk strings are quoted; nil shows as `(nil)`.
pub fn format_reply(reply: &Reply, indent: usize) -> String {
    match reply {
        Reply::Simple(s) => String::from_utf8_lossy(s).into_owned(),
        Reply::Error(s) => format!("(error) {}", String::from_utf8_lossy(s)),
        Reply::Int(n) => format!("(integer) {n}"),
        Reply::Bulk(b) => format!("\"{}\"", String::from_utf8_lossy(b)),
        Reply::Nil => "(nil)".to_string(),
        Reply::Array(items) if items.is_empty() => "(empty array)".to_string(),
        Reply::Array(items) => {
            let pad = "   ".repeat(indent);
            items
                .iter()
                .enumerate()
                .map(|(i, it)| format!("{pad}{}) {}", i + 1, format_reply(it, indent + 1)))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}
