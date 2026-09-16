//! One connection to a RESP server, reporting failures in hiredis's words.
//!
//! redis-cli prints hiredis's `errstr` — `Connection refused`, `Server closed
//! the connection` — and a script that matches on those lines should keep
//! working, so the errors here are that text rather than Rust's `Display`,
//! which appends ` (os error N)`.

use kevy_resp::{Reply, encode_command};
use kevy_resp_client::ReplyReadBuf;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Why a read or write on an open connection failed.
#[derive(Debug)]
pub(crate) enum LinkError {
    /// The peer closed the connection (`REDIS_ERR_EOF`).
    Eof,
    /// A socket error (`REDIS_ERR_IO`), with its `strerror` text.
    Io(io::ErrorKind, String),
    /// Bytes that are not RESP.
    Protocol,
}

impl LinkError {
    /// hiredis's `errstr` for this failure.
    pub(crate) fn text(&self) -> String {
        match self {
            LinkError::Eof => "Server closed the connection".to_string(),
            LinkError::Io(_, text) => text.clone(),
            LinkError::Protocol => "Protocol error".to_string(),
        }
    }

    /// The failures redis-cli's REPL survives by reconnecting.
    pub(crate) fn is_reconnectable(&self) -> bool {
        matches!(self, LinkError::Eof)
            || matches!(self, LinkError::Io(k, _) if matches!(k, io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe))
    }
}

/// A reply and the wire text of each double in it.
pub(crate) type Parsed = (Reply, Vec<Vec<u8>>);

enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

/// An open connection plus its reply buffer.
pub(crate) struct Conn {
    stream: Stream,
    buf: ReplyReadBuf,
    chunk: Box<[u8]>,
}

/// `strerror` for an I/O error: Rust's text without ` (os error N)`.
pub(crate) fn strerror(e: &io::Error) -> String {
    let text = e.to_string();
    let text = text.strip_prefix("failed to lookup address information: ").unwrap_or(&text);
    match text.rfind(" (os error ") {
        Some(cut) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

impl Conn {
    /// `redisConnectWrapper`: TCP, optionally with a connect timeout.
    pub(crate) fn tcp(host: &[u8], port: i32, timeout: Option<f64>) -> Result<Conn, String> {
        let host = String::from_utf8_lossy(host).into_owned();
        let port = u16::try_from(port).map_err(|_| "Invalid port".to_string())?;
        let stream = match timeout {
            None => TcpStream::connect((host.as_str(), port)).map_err(|e| strerror(&e))?,
            Some(secs) => connect_with_timeout(&host, port, Duration::from_secs_f64(secs))?,
        };
        let _ = stream.set_nodelay(true); // latency tuning only; a refusal changes nothing a user sees
        Ok(Conn::over(Stream::Tcp(stream)))
    }

    /// `redisConnectUnixWrapper`.
    pub(crate) fn unix(path: &[u8]) -> Result<Conn, String> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::OsStr::from_bytes(path);
        UnixStream::connect(path).map(|s| Conn::over(Stream::Unix(s))).map_err(|e| strerror(&e))
    }

    fn over(stream: Stream) -> Conn {
        Conn {
            stream,
            buf: ReplyReadBuf::with_capacity(16 * 1024),
            chunk: vec![0; 16 * 1024].into_boxed_slice(),
        }
    }

    /// Write one command.
    pub(crate) fn send(&mut self, argv: &[Vec<u8>]) -> Result<(), LinkError> {
        let mut frame = Vec::new();
        encode_command(&mut frame, argv);
        self.write_raw(&frame)
    }

    /// Write bytes as they are.
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        let r = match &mut self.stream {
            Stream::Tcp(s) => s.write_all(bytes),
            Stream::Unix(s) => s.write_all(bytes),
        };
        r.map_err(|e| LinkError::Io(e.kind(), strerror(&e)))
    }

    /// Block until one whole reply is buffered; return it with the text of
    /// each double in it.
    pub(crate) fn read_reply(&mut self) -> Result<Parsed, LinkError> {
        loop {
            if let Some(parsed) = self.buffered_reply()? {
                return Ok(parsed);
            }
            let n = match &mut self.stream {
                Stream::Tcp(s) => s.read(&mut self.chunk),
                Stream::Unix(s) => s.read(&mut self.chunk),
            }
            .map_err(|e| LinkError::Io(e.kind(), strerror(&e)))?;
            if n == 0 {
                return Err(LinkError::Eof);
            }
            self.buf.extend(&self.chunk[..n]);
        }
    }

    /// A reply already buffered, without reading (`redisGetReplyFromReader`).
    pub(crate) fn buffered_reply(&mut self) -> Result<Option<Parsed>, LinkError> {
        match self.buf.parse_next_keeping_double_text() {
            Ok(parsed) => Ok(parsed.map(|(reply, _, texts)| (reply, texts))),
            Err(_) => Err(LinkError::Protocol),
        }
    }

    /// The socket, for waiting on it together with stdin.
    pub(crate) fn fd(&self) -> RawFd {
        match &self.stream {
            Stream::Tcp(s) => s.as_raw_fd(),
            Stream::Unix(s) => s.as_raw_fd(),
        }
    }
}

/// Resolve, then try each address within `timeout`.
fn connect_with_timeout(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let addrs = (host, port).to_socket_addrs().map_err(|e| strerror(&e))?;
    let mut last = String::from("Connection refused");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = strerror(&e),
        }
    }
    Err(last)
}
