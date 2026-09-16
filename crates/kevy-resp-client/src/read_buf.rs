//! [`ReplyReadBuf`] — a read buffer with a consume cursor, shared by
//! every kevy client that drains RESP replies off a stream.
//!
//! The naive loop is `parse_reply(&buf)` then `buf.drain(..used)` per
//! reply. `drain(..used)` memmoves the whole unconsumed tail down to the
//! front — O(remaining) — which turns a pipelined read of N replies into
//! O(N²). This type keeps a `pos` cursor instead: parsing advances past
//! each reply in O(1), and the consumed prefix is reclaimed lazily (on a
//! full drain, when the cursor passes the halfway mark, or right before a
//! large append). The classic hiredis idiom, in safe index math — no
//! `unsafe`.

use kevy_resp::{ProtocolError, Reply, TextedReply, parse_reply, parse_reply_keeping_double_text};

/// A growable read buffer that parses replies off its front via a
/// consume cursor rather than front-draining after every reply.
///
/// One per connection; holds partial replies across `read` calls.
#[derive(Debug)]
pub struct ReplyReadBuf {
    buf: Vec<u8>,
    /// Byte offset of the first unconsumed byte. Everything in
    /// `buf[..pos]` has already been parsed and handed back.
    pos: usize,
}

impl ReplyReadBuf {
    /// New buffer preallocated to `cap` bytes.
    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: Vec::with_capacity(cap), pos: 0 }
    }

    /// Parse one reply out of the unconsumed region.
    ///
    /// - `Ok(Some(reply))` — one complete reply was parsed; the cursor
    ///   advanced past it (and the consumed prefix may have been
    ///   reclaimed).
    /// - `Ok(None)` — the buffer holds only a partial reply; feed more
    ///   bytes with [`Self::extend`] and call again.
    /// - `Err(_)` — the bytes are a malformed RESP frame.
    pub fn parse_next(&mut self) -> Result<Option<Reply>, ProtocolError> {
        match parse_reply(&self.buf[self.pos..])? {
            Some((reply, used)) => {
                self.pos += used;
                self.reclaim_after_consume();
                Ok(Some(reply))
            }
            None => Ok(None),
        }
    }

    /// [`Self::parse_next`], also returning the wire text of each double in
    /// the reply (see [`kevy_resp::parse_reply_keeping_double_text`]). The
    /// `usize` is how many buffered bytes the reply took.
    pub fn parse_next_keeping_double_text(&mut self) -> Result<Option<TextedReply>, ProtocolError> {
        let parsed = parse_reply_keeping_double_text(&self.buf[self.pos..])?;
        if let Some((_, used, _)) = &parsed {
            self.pos += used;
            self.reclaim_after_consume();
        }
        Ok(parsed)
    }

    /// The buffered bytes not yet consumed by a parse.
    ///
    /// For reporting a malformed frame the way a client library does — by
    /// the byte it could not read — without re-reading the socket.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_resp_client::ReplyReadBuf;
    ///
    /// let mut buf = ReplyReadBuf::with_capacity(16);
    /// buf.extend(b"+OK\r\n@bad");
    /// assert!(buf.parse_next().is_ok());
    /// assert_eq!(buf.pending(), b"@bad");
    /// assert!(buf.parse_next().is_err());
    /// ```
    pub fn pending(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Append freshly-read bytes. Compacts the consumed prefix first when
    /// the append is large relative to what's still buffered, so a deep
    /// pipeline can't grow the allocation without bound.
    pub fn extend(&mut self, bytes: &[u8]) {
        if self.pos > 0 && bytes.len() > self.buf.len() - self.pos {
            self.compact();
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Reclaim the consumed prefix after advancing the cursor: reset to
    /// the front on a full drain (cheap, keeps the allocation), else
    /// compact once the cursor is past the halfway mark.
    fn reclaim_after_consume(&mut self) {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > self.buf.capacity() / 2 {
            self.compact();
        }
    }

    /// Drop the consumed prefix, sliding the tail to the front.
    fn compact(&mut self) {
        self.buf.drain(..self.pos);
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sequential_replies_via_cursor() {
        let mut b = ReplyReadBuf::with_capacity(64);
        b.extend(b"+A\r\n+B\r\n");
        assert!(matches!(b.parse_next(), Ok(Some(Reply::Simple(s))) if s == b"A"));
        assert!(matches!(b.parse_next(), Ok(Some(Reply::Simple(s))) if s == b"B"));
        // Fully drained → cursor reset, no bytes left to parse.
        assert!(matches!(b.parse_next(), Ok(None)));
    }

    #[test]
    fn partial_reply_returns_none_then_completes() {
        let mut b = ReplyReadBuf::with_capacity(16);
        b.extend(b"$5\r\nhel");
        assert!(matches!(b.parse_next(), Ok(None)));
        b.extend(b"lo\r\n");
        assert!(matches!(b.parse_next(), Ok(Some(Reply::Bulk(v))) if v == b"hello"));
    }

    #[test]
    fn survives_compaction_mid_stream() {
        // Small capacity so the halfway-mark compaction path fires while
        // a trailing partial reply is still buffered.
        let mut b = ReplyReadBuf::with_capacity(8);
        b.extend(b"+first-longish\r\n$3\r\nabc");
        assert!(matches!(b.parse_next(), Ok(Some(Reply::Simple(s))) if s == b"first-longish"));
        // Trailing partial `$3\r\nabc` (missing CRLF) must survive the
        // compaction that the long consumed prefix triggered.
        assert!(matches!(b.parse_next(), Ok(None)));
        b.extend(b"\r\n");
        assert!(matches!(b.parse_next(), Ok(Some(Reply::Bulk(v))) if v == b"abc"));
    }

    #[test]
    fn malformed_frame_errors() {
        let mut b = ReplyReadBuf::with_capacity(16);
        b.extend(b"!bogus\r\n");
        assert!(b.parse_next().is_err());
    }

    #[test]
    fn double_text_survives_a_split_read() {
        let mut b = ReplyReadBuf::with_capacity(8);
        b.extend(b",1e+3");
        assert!(matches!(b.parse_next_keeping_double_text(), Ok(None)));
        b.extend(b"00\r\n+next\r\n");
        let (reply, _, texts) = b.parse_next_keeping_double_text().unwrap().unwrap();
        assert_eq!(reply, Reply::Double(1e300));
        assert_eq!(texts, [b"1e+300".to_vec()]);
        assert!(matches!(b.parse_next(), Ok(Some(Reply::Simple(s))) if s == b"next"));
    }
}
