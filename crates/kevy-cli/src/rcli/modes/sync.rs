//! Starting a replication SYNC as a client: the REPLCONF handshake, the raw
//! SYNC request, and the header that says how the payload is delimited.

use crate::rcli::conn::LinkError;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// How the snapshot payload ends.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Payload {
    /// A diskless transfer: it ends with this 40-byte mark.
    UntilMark(Vec<u8>),
    /// A transfer of known length.
    Length(u64),
}

/// `REPLCONF <name> <value>`, said on stderr first. `Err` carries the error
/// reply, which the caller decides about.
pub(crate) fn replconf(s: &mut Session, name: &[u8], value: &[u8]) -> Result<(), Vec<u8>> {
    eprint_bytes(&[b"sending REPLCONF ", name, b" ", value, b"\n"]);
    match s.request(&[b"REPLCONF", name, value]) {
        Ok(Reply::Error(msg)) => {
            eprint_bytes(&[b"REPLCONF ", name, b" error: ", &msg, b"\n"]);
            Err(msg)
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e.text().into_bytes()),
    }
}

/// Send SYNC and read the payload header, skipping the newlines a master
/// sends while it prepares the snapshot.
pub(crate) fn start(s: &mut Session) -> Result<Payload, Vec<u8>> {
    let conn = s.conn.as_mut().ok_or_else(|| b"Error: not connected".to_vec())?;
    conn.write_raw(b"SYNC\r\n")
        .map_err(|e| [b"Error writing to master: ".as_slice(), e.text().as_bytes()].concat())?;
    loop {
        let line =
            read_line(conn).map_err(|_| b"Error reading bulk length while SYNCing".to_vec())?;
        // The failure is quoted with its line end, `\r` included.
        let bare = line.strip_suffix(b"\r").unwrap_or(&line);
        match bare.first() {
            None => continue,
            Some(b'+') => eprint_bytes(&[b"PSYNC replied ", bare, b"\n"]),
            Some(b'$') => return header(&bare[1..]),
            Some(_) => return Err([b"SYNC with master failed: ".as_slice(), &line].concat()),
        }
    }
}

fn header(rest: &[u8]) -> Result<Payload, Vec<u8>> {
    if let Some(mark) = rest.strip_prefix(b"EOF:") {
        return Ok(Payload::UntilMark(mark.to_vec()));
    }
    let digits = std::str::from_utf8(rest).ok().and_then(|t| t.parse::<u64>().ok());
    digits
        .map(Payload::Length)
        .ok_or_else(|| [b"SYNC with master failed: $".as_slice(), rest].concat())
}

/// One line, without its `\n`, a byte at a time so nothing past it is taken.
fn read_line(conn: &mut crate::rcli::conn::Conn) -> Result<Vec<u8>, LinkError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        conn.read_raw(&mut byte)?;
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
    }
}

/// Where payload bytes go; `Err` carries the message that ends the transfer.
pub(crate) type Sink<'a> = dyn FnMut(&[u8]) -> Result<(), Vec<u8>> + 'a;

/// Copy the payload to `sink` (or nowhere), handing anything read past its
/// end back to reply parsing; the payload's length.
pub(crate) fn transfer(
    s: &mut Session,
    payload: &Payload,
    sink: &mut Sink<'_>,
) -> Result<u64, Vec<u8>> {
    let conn = s.conn.as_mut().ok_or_else(|| b"Error: not connected".to_vec())?;
    let mut chunk = vec![0u8; 64 * 1024];
    let read_failed =
        |e: LinkError| [b"Error reading from master: ".as_slice(), e.text().as_bytes()].concat();
    match payload {
        Payload::Length(total) => {
            let mut left = *total;
            while left > 0 {
                let want = usize::try_from(left).unwrap_or(usize::MAX).min(chunk.len());
                let n = conn.read_raw(&mut chunk[..want]).map_err(read_failed)?;
                sink(&chunk[..n])?;
                left -= n as u64;
            }
            Ok(*total)
        }
        Payload::UntilMark(mark) => {
            // The mark can straddle reads: hold back the last mark-length
            // bytes until more arrive or they turn out to be the mark.
            let (mut held, mut written) = (Vec::new(), 0u64);
            loop {
                let n = conn.read_raw(&mut chunk).map_err(read_failed)?;
                held.extend_from_slice(&chunk[..n]);
                if let Some(end) = held.windows(mark.len()).position(|w| w == mark.as_slice()) {
                    sink(&held[..end])?;
                    conn.unread(&held[end + mark.len()..]);
                    return Ok(written + end as u64);
                }
                let keep = held.len().min(mark.len() - 1);
                let flush = held.len() - keep;
                sink(&held[..flush])?;
                written += flush as u64;
                held.drain(..flush);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Payload, header};

    #[test]
    fn payload_headers() {
        assert_eq!(header(b"EOF:0123"), Ok(Payload::UntilMark(b"0123".to_vec())));
        assert_eq!(header(b"253"), Ok(Payload::Length(253)));
        assert!(header(b"x").is_err());
    }
}
