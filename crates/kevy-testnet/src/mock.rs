//! The reading half of a hand-written mock server.

use std::io::Read;
use std::net::TcpStream;

/// Read one whole RESP request (an array of bulk strings) off `sock`,
/// keeping any bytes past it in `pending` for the next call. `false` = the
/// client closed, or went quiet past the socket's read timeout, first.
///
/// Mocks that instead waited for a hand-counted number of bytes waited out
/// their read timeout whenever the count was off by one, and passed anyway.
///
/// ```
/// use std::io::Write;
/// let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
/// let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
/// client.write_all(b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n").unwrap();
/// let (mut sock, _) = listener.accept().unwrap();
/// let mut pending = Vec::new();
/// assert!(kevy_testnet::read_request(&mut sock, &mut pending));
/// assert!(kevy_testnet::read_request(&mut sock, &mut pending), "the second came in the same read");
/// assert!(pending.is_empty());
/// ```
pub fn read_request(sock: &mut TcpStream, pending: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 1024];
    loop {
        if let Some(len) = request_len(pending) {
            pending.drain(..len);
            return true;
        }
        match sock.read(&mut buf) {
            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
            _ => return false,
        }
    }
}

/// The length of the complete request at the front of `b`, if one is there.
fn request_len(b: &[u8]) -> Option<usize> {
    let header = |at: usize| -> Option<(usize, usize)> {
        let end = at + b.get(at..)?.windows(2).position(|w| w == b"\r\n")?;
        let n = std::str::from_utf8(b.get(at + 1..end)?).ok()?.parse().ok()?;
        Some((n, end + 2))
    };
    let (count, mut at) = header(0)?;
    for _ in 0..count {
        let (len, body) = header(at)?;
        at = body + len + 2;
    }
    (at <= b.len()).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::{read_request, request_len};
    use std::io::Write;

    #[test]
    fn a_client_that_leaves_mid_request_sent_none() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.write_all(b"*2\r\n$3\r\nGET\r\n").unwrap();
        drop(client);
        let (mut sock, _) = listener.accept().unwrap();
        let mut pending = Vec::new();
        assert!(!read_request(&mut sock, &mut pending));
        assert_eq!(pending, b"*2\r\n$3\r\nGET\r\n", "what did arrive is kept");
    }

    #[test]
    fn a_header_that_is_not_a_count_never_completes() {
        assert_eq!(request_len(b"*x\r\n"), None);
        assert_eq!(request_len(b"*\xff\r\n"), None);
        assert_eq!(request_len(b"*1\r\n$y\r\nab\r\n"), None);
        assert_eq!(request_len(b"\r\n"), None, "an empty header line");
        assert_eq!(request_len(b"*1\r\n\r\nab\r\n"), None, "an empty element header");
    }

    #[test]
    fn a_request_is_complete_only_with_its_last_crlf() {
        let whole = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
        assert_eq!(request_len(whole), Some(whole.len()));
        for cut in 0..whole.len() {
            assert_eq!(request_len(&whole[..cut]), None, "cut at {cut}");
        }
    }
}
