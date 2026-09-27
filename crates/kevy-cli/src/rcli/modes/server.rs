//! The questions every keyspace report asks first: how many keys, and
//! whether a cluster replica may be read.

use crate::rcli::session::Session;
use kevy_resp::Reply;

/// DBSIZE, or the message that ends the report.
pub(crate) fn key_count(s: &mut Session) -> Result<u64, Vec<u8>> {
    match s.request(&[b"DBSIZE"]) {
        Ok(Reply::Int(n)) => Ok(n.max(0) as u64),
        Ok(Reply::Error(msg)) => Err([b"Couldn't determine DBSIZE: ".as_slice(), &msg].concat()),
        Ok(_) => Err(b"Non INTEGER response from DBSIZE!".to_vec()),
        Err(_) => Err(b"\nI/O error".to_vec()),
    }
}

/// READONLY, so a cluster replica answers; a server without cluster support,
/// or without the command, is fine as it is.
pub(crate) fn allow_replica_reads(s: &mut Session) -> Result<(), Vec<u8>> {
    match s.request(&[b"READONLY"]) {
        Ok(Reply::Error(msg))
            if msg != b"ERR This instance has cluster support disabled"
                && !msg.starts_with(b"ERR unknown command") =>
        {
            Err([b"Error: ".as_slice(), &msg].concat())
        }
        Ok(_) => Ok(()),
        Err(_) => Err(b"\nI/O error".to_vec()),
    }
}

/// `%-<width>s` / `%<width>s` on bytes.
pub(crate) fn pad(text: &[u8], width: usize, align: Align) -> Vec<u8> {
    let fill = vec![b' '; width.saturating_sub(text.len())];
    match align {
        Align::Left => [text, &fill].concat(),
        Align::Right => [&fill, text].concat(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Right,
}
