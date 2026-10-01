//! One connection to a RESP server, reporting failures in redis-cli's words.
//!
//! redis-cli prints `Connection refused`, `Server closed
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
    /// Bytes that are not RESP; the byte where a reply should have started,
    /// when that is where it went wrong.
    Protocol(Option<u8>),
}

impl LinkError {
    /// What redis-cli prints for this failure.
    pub(crate) fn text(&self) -> String {
        match self {
            LinkError::Eof => "Server closed the connection".to_string(),
            LinkError::Io(_, text) => text.clone(),
            // redis-cli: `Protocol error, got "<byte>" as reply type byte`.
            LinkError::Protocol(Some(byte)) => {
                let shown = String::from_utf8_lossy(&super::repr::repr(&[*byte])).into_owned();
                format!("Protocol error, got {shown} as reply type byte")
            }
            LinkError::Protocol(None) => "Protocol error".to_string(),
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
    /// kevy's encrypted client port (`-u kevys://…`).
    Secure(Box<kevy_resp_client::SecureStream>),
}

/// An open connection plus its reply buffer.
pub(crate) struct Conn {
    stream: Stream,
    buf: ReplyReadBuf,
    chunk: Box<[u8]>,
}

/// `strerror` for an I/O error: Rust's text without ` (os error N)`.
pub(crate) fn strerror(e: &io::Error) -> String {
    // `connect_timeout` reports its own timeout without an errno, in
    // lowercase; redis-cli prints `Connection timed out`.
    if e.kind() == io::ErrorKind::TimedOut && e.raw_os_error().is_none() {
        return "Connection timed out".to_string();
    }
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
        // redis-cli keeps only the low 16 bits of the port, so `connect h
        // 99999` in the REPL dials 34463 — and the message still names 99999.
        // Only the REPL can hand over such a port.
        Ok(Conn::over(Stream::Tcp(dial_tcp(host, port as u16, timeout)?)))
    }

    /// Dial like [`Conn::tcp`], then run the Noise handshake that kevy's
    /// encrypted client port expects.
    pub(crate) fn secure(
        host: &[u8],
        port: i32,
        timeout: Option<f64>,
        target: &super::opts::SecureTarget,
    ) -> Result<Conn, String> {
        let me = match &target.client_key_file {
            Some(path) => Some(
                kevy_resp_client::load_client_key(path)
                    .map_err(|e| format!("client key {}: {}", path.display(), strerror(&e)))?,
            ),
            None => None,
        };
        let tcp = dial_tcp(host, port as u16, timeout)?;
        let s = kevy_resp_client::SecureStream::handshake(tcp, target.server_key, me.as_ref())
            .map_err(|e| strerror(&e))?;
        Ok(Conn::over(Stream::Secure(Box::new(s))))
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

    /// Send `argv` and read its reply; pushes that arrive first are skipped.
    pub(crate) fn request(&mut self, argv: &[&[u8]]) -> Result<Reply, LinkError> {
        let mut replies = self.pipeline(&[argv.to_vec()])?;
        replies.pop().ok_or(LinkError::Eof)
    }

    /// Send every command in one write, then read one reply for each, in
    /// order: a batch costs one round trip, not one per command.
    pub(crate) fn pipeline(&mut self, commands: &[Vec<&[u8]>]) -> Result<Vec<Reply>, LinkError> {
        let mut frame = Vec::new();
        for argv in commands {
            kevy_resp::encode_command_borrowed(&mut frame, argv);
        }
        self.write_raw(&frame)?;
        let mut replies = Vec::with_capacity(commands.len());
        while replies.len() < commands.len() {
            match self.read_reply()? {
                (Reply::Push(_), _) => {}
                (reply, _) => replies.push(reply),
            }
        }
        Ok(replies)
    }

    /// Write bytes as they are.
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        let r = match &mut self.stream {
            Stream::Tcp(s) => s.write_all(bytes),
            Stream::Unix(s) => s.write_all(bytes),
            Stream::Secure(s) => s.write_all(bytes),
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
            let n = self.read_stream().map_err(|e| LinkError::Io(e.kind(), strerror(&e)))?;
            if n == 0 {
                return Err(LinkError::Eof);
            }
        }
    }

    /// One read, appended to the reply buffer; `0` at end of stream. An
    /// encrypted stream may hold decrypted bytes beyond what the chunk took;
    /// they are appended too, so none wait where a caller polling the
    /// socket could not see them.
    fn read_stream(&mut self) -> io::Result<usize> {
        let n = match &mut self.stream {
            Stream::Tcp(s) => s.read(&mut self.chunk)?,
            Stream::Unix(s) => s.read(&mut self.chunk)?,
            Stream::Secure(s) => s.read(&mut self.chunk)?,
        };
        self.buf.extend(&self.chunk[..n]);
        if let Stream::Secure(s) = &mut self.stream {
            let mut more = vec![0u8; s.buffered()];
            s.read_exact(&mut more)?;
            self.buf.extend(&more);
        }
        Ok(n)
    }

    /// A reply already buffered, without reading (`redisGetReplyFromReader`).
    pub(crate) fn buffered_reply(&mut self) -> Result<Option<Parsed>, LinkError> {
        match self.buf.parse_next_keeping_double_text() {
            Ok(parsed) => Ok(parsed.map(|(reply, _, texts)| (reply, texts))),
            Err(_) => {
                const TYPE_BYTES: &[u8] = b"+-:$*%~,#=(_>!|";
                let first = self.buf.pending().first().copied();
                Err(LinkError::Protocol(first.filter(|b| !TYPE_BYTES.contains(b))))
            }
        }
    }

    /// Bytes straight from the socket, bypassing reply parsing (a replication
    /// payload). Only once every reply before it has been read.
    pub(crate) fn read_raw(&mut self, into: &mut [u8]) -> Result<usize, LinkError> {
        let n = match &mut self.stream {
            Stream::Tcp(s) => s.read(into),
            Stream::Unix(s) => s.read(into),
            Stream::Secure(s) => s.read(into),
        }
        .map_err(|e| LinkError::Io(e.kind(), strerror(&e)))?;
        if n == 0 { Err(LinkError::Eof) } else { Ok(n) }
    }

    /// Hand bytes read raw back to reply parsing.
    pub(crate) fn unread(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
    }

    /// A second handle on the socket for another thread to write through.
    pub(crate) fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(match &self.stream {
            Stream::Tcp(s) => Box::new(s.try_clone()?),
            Stream::Unix(s) => Box::new(s.try_clone()?),
            Stream::Secure(s) => Box::new(s.writer()?),
        })
    }

    /// Bound how long a read may wait; `None` waits for ever.
    pub(crate) fn set_read_timeout(&self, limit: Option<Duration>) -> io::Result<()> {
        match &self.stream {
            Stream::Tcp(s) => s.set_read_timeout(limit),
            Stream::Unix(s) => s.set_read_timeout(limit),
            Stream::Secure(s) => s.socket().set_read_timeout(limit),
        }
    }

    /// The socket, for waiting on it together with stdin.
    pub(crate) fn fd(&self) -> RawFd {
        match &self.stream {
            Stream::Tcp(s) => s.as_raw_fd(),
            Stream::Unix(s) => s.as_raw_fd(),
            Stream::Secure(s) => s.socket().as_raw_fd(),
        }
    }
}

/// `redisConnectWrapper`'s dial: TCP, optionally with a connect timeout.
fn dial_tcp(host: &[u8], port: u16, timeout: Option<f64>) -> Result<TcpStream, String> {
    let host = String::from_utf8_lossy(host).into_owned();
    let stream = match timeout {
        None => TcpStream::connect((host.as_str(), port)).map_err(|e| strerror(&e))?,
        Some(secs) => connect_with_timeout(&host, port, Duration::from_secs_f64(secs))?,
    };
    let _ = stream.set_nodelay(true); // latency tuning only; a refusal changes nothing a user sees
    Ok(stream)
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

/// The tools' request/reply interface over this connection; push messages
/// that arrive before a reply are skipped, as [`Conn::pipeline`] skips them.
impl crate::link::Link for Conn {
    fn request_borrowed(&mut self, argv: &[&[u8]]) -> std::io::Result<Reply> {
        self.request(argv).map_err(link_error)
    }

    fn pipeline_raw(&mut self, raw: &[u8], n: usize) -> std::io::Result<Vec<Reply>> {
        self.write_raw(raw).map_err(link_error)?;
        let mut replies = Vec::with_capacity(n);
        while replies.len() < n {
            match self.read_reply().map_err(link_error)? {
                (Reply::Push(_), _) => {}
                (reply, _) => replies.push(reply),
            }
        }
        Ok(replies)
    }
}

fn link_error(e: LinkError) -> std::io::Error {
    match e {
        LinkError::Io(kind, text) => std::io::Error::new(kind, text),
        LinkError::Protocol(_) => std::io::Error::new(std::io::ErrorKind::InvalidData, e.text()),
        LinkError::Eof => std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e.text()),
    }
}

#[cfg(test)]
mod tests {
    use super::{LinkError, link_error};
    use std::io::ErrorKind;

    #[test]
    fn a_lost_link_keeps_its_kind_when_a_tool_reads_it() {
        let io = link_error(LinkError::Io(ErrorKind::ConnectionReset, "Connection reset".into()));
        assert_eq!(
            (io.kind(), io.to_string()),
            (ErrorKind::ConnectionReset, "Connection reset".into())
        );
        assert_eq!(link_error(LinkError::Eof).kind(), ErrorKind::UnexpectedEof);
        // Bytes that are not RESP are data, not a closed socket.
        assert_eq!(link_error(LinkError::Protocol(Some(b'x'))).kind(), ErrorKind::InvalidData);
    }

    /// A connection to a one-shot Noise responder that answers the first
    /// request with `replies`, all sealed as ONE message, and then waits.
    fn secure_conn_answering(replies: Vec<u8>) -> super::Conn {
        use kevy_noise::{Frames, Keypair, Responder, frame};
        use std::io::{Read, Write};
        let key = Keypair::from_secret([1; 32]);
        let public = key.public();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (mut frames, mut chunk) = (Frames::default(), [0u8; 4096]);
            let mut next = |s: &mut std::net::TcpStream, f: &mut Frames| loop {
                if let Some(m) = f.next() {
                    return m;
                }
                let n = s.read(&mut chunk).unwrap();
                f.push(&chunk[..n]);
            };
            let m1 = next(&mut s, &mut frames);
            let prologue = b"kevy-client\x001";
            let (_, r) =
                Responder::accept(&key, Keypair::from_secret([2; 32]), prologue, &m1).unwrap();
            let (m2, mut t) = r.finish(b"").unwrap();
            s.write_all(&frame(&m2).unwrap()).unwrap();
            let _request = t.open(&next(&mut s, &mut frames)).unwrap();
            s.write_all(&frame(&t.seal(&replies).unwrap()).unwrap()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(5)); // hold the socket open
        });
        let target = super::super::opts::SecureTarget { server_key: public, client_key_file: None };
        super::Conn::secure(b"127.0.0.1", i32::from(port), None, &target).unwrap()
    }

    #[test]
    fn one_read_leaves_no_decrypted_bytes_behind_in_the_stream() {
        let replies = b"+OK\r\n".repeat(4000); // 20 KB, more than one read takes
        let mut conn = secure_conn_answering(replies);
        conn.send(&[b"PING".to_vec()]).unwrap();
        assert!(conn.read_reply().is_ok());
        // the other 3999 are parsed from the buffer alone: a caller that
        // waited on the socket for them would wait for ever
        for i in 1..4000 {
            assert!(matches!(conn.buffered_reply(), Ok(Some(_))), "reply {i} was not buffered");
        }
    }
}
