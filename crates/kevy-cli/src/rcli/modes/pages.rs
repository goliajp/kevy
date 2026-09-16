//! The keyspace a page at a time: SCAN from a cursor until it comes back 0.

use crate::rcli::session::Session;
use kevy_resp::Reply;

/// Why a page could not be read, in the words a mode reports.
#[derive(Debug)]
pub(crate) enum PageError {
    /// The server answered SCAN with an error.
    Refused(Vec<u8>),
    /// SCAN's reply was not a two-element array of cursor and keys.
    NotArray,
    WrongLength,
    /// The connection failed.
    Link,
}

impl PageError {
    /// What redis-cli prints for it.
    pub(crate) fn text(&self) -> Vec<u8> {
        match self {
            PageError::Refused(msg) => [b"SCAN error: ".as_slice(), msg].concat(),
            PageError::NotArray => b"Non ARRAY response from SCAN!".to_vec(),
            PageError::WrongLength => b"Invalid element count from SCAN!".to_vec(),
            PageError::Link => b"\nI/O error".to_vec(),
        }
    }
}

/// Where a SCAN walk is, and what it asks for.
pub(crate) struct Pages {
    cursor: u64,
    pattern: Option<Vec<u8>>,
    count: Vec<u8>,
    finished: bool,
}

impl Pages {
    pub(crate) fn new(cursor: u64, pattern: Option<Vec<u8>>, count: i32) -> Pages {
        Pages { cursor, pattern, count: count.to_string().into_bytes(), finished: false }
    }

    /// The next page of keys; `None` once the walk is back at cursor 0.
    pub(crate) fn next(&mut self, s: &mut Session) -> Result<Option<Vec<Vec<u8>>>, PageError> {
        if self.finished {
            return Ok(None);
        }
        let cursor = self.cursor.to_string();
        let mut argv: Vec<&[u8]> = vec![b"SCAN", cursor.as_bytes()];
        if let Some(p) = &self.pattern {
            argv.extend_from_slice(&[b"MATCH", p]);
        }
        argv.extend_from_slice(&[b"COUNT", &self.count]);
        let (cursor, keys) = match s.request(&argv).map_err(|_| PageError::Link)? {
            Reply::Error(msg) => return Err(PageError::Refused(msg)),
            Reply::Array(parts) if parts.len() == 2 => split(parts)?,
            Reply::Array(_) => return Err(PageError::WrongLength),
            _ => return Err(PageError::NotArray),
        };
        self.cursor = cursor_value(&cursor);
        self.finished = self.cursor == 0;
        Ok(Some(keys))
    }
}

/// `[cursor, [key, …]]` into its parts; anything but strings is not a key.
fn split(parts: Vec<Reply>) -> Result<(Vec<u8>, Vec<Vec<u8>>), PageError> {
    let mut parts = parts.into_iter();
    let (Some(Reply::Bulk(cursor)), Some(Reply::Array(keys))) = (parts.next(), parts.next()) else {
        return Err(PageError::NotArray);
    };
    let keys = keys
        .into_iter()
        .map(|k| match k {
            Reply::Bulk(b) | Reply::Simple(b) => Ok(b),
            _ => Err(PageError::NotArray),
        })
        .collect::<Result<_, _>>()?;
    Ok((cursor, keys))
}

/// A cursor as `strtoull` reads it: leading digits, saturating, 0 for none.
fn cursor_value(text: &[u8]) -> u64 {
    text.iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0u64, |acc, d| acc.saturating_mul(10).saturating_add(u64::from(d - b'0')))
}
