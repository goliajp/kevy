//! The two ends of a batch: the reactor parsing a connection's commands
//! and dispatching each, and the owning shard prefetching the keys of a
//! batch forwarded to it.

use kevy_resp::parse_command_borrowed;

use crate::Commands;
use crate::message_kinds::DispatchMeta;
use crate::shard::Shard;

/// How many requests ahead of the one running the owner prefetches: far
/// enough that a bucket arrives from memory before its request runs, near
/// enough that the lines in flight fit the core's miss buffers.
pub(crate) const PREFETCH_AHEAD: usize = 4;

/// What [`Shard::dispatch_batch`] saw: how far the parse cursor got,
/// whether it stopped on malformed input, and whether the conn was
/// closed by one of its own commands (QUIT) mid-batch.
pub(crate) struct BatchOutcome {
    pub(crate) consumed: usize,
    pub(crate) protocol_error: bool,
    pub(crate) conn_gone: bool,
}

/// A key's route, worked out a command ahead, and the bytes it is for.
#[derive(Clone, Copy)]
pub(crate) struct RouteHint {
    at: usize,
    len: usize,
    pub(crate) shard: usize,
    pub(crate) hash: Option<u64>,
}

impl RouteHint {
    /// Whether this is the route of exactly `key`: the same bytes of the
    /// same batch buffer, which nothing changes while the batch runs.
    #[inline]
    pub(crate) fn is(&self, key: &[u8]) -> bool {
        self.at == key.as_ptr() as usize && self.len == key.len()
    }
}

impl<C: Commands> Shard<C> {
    /// Parse and dispatch every complete RESP command at the front of
    /// `buf` (the borrowed-argv hot path shared by both reactors). The
    /// caller owns buffer bookkeeping (tail retention) and the AOF
    /// group-commit window around the batch.
    pub(crate) fn dispatch_batch(&mut self, conn_id: u64, buf: &[u8]) -> BatchOutcome {
        let mut off = 0usize;
        let mut route = peek_key(buf).map(|k| self.prefetch_local(k));
        loop {
            match parse_command_borrowed(&buf[off..]) {
                Ok(Some((argv, consumed))) => {
                    // the next command's bucket is fetched while this one
                    // runs, when its key is this shard's
                    let next = peek_key(&buf[off + consumed..]).map(|k| self.prefetch_local(k));
                    self.route_hint = route;
                    self.handle_command(conn_id, &argv);
                    self.route_hint = None;
                    drop(argv);
                    off += consumed;
                    if crate::conn::conn_at(&mut self.conns, &mut self.conn_slot_hint, conn_id)
                        .is_none()
                    {
                        return BatchOutcome {
                            consumed: off,
                            protocol_error: false,
                            conn_gone: true,
                        };
                    }
                    route = next;
                }
                Ok(None) => {
                    return BatchOutcome { consumed: off, protocol_error: false, conn_gone: false };
                }
                Err(_) => {
                    return BatchOutcome { consumed: off, protocol_error: true, conn_gone: false };
                }
            }
        }
    }

    /// Route `key` (a command's argv[1], where nearly every keyed command
    /// has it) and, if this shard owns it, start fetching its bucket. Keys
    /// of other shards are left to their owner, which prefetches its own.
    #[inline(always)]
    fn prefetch_local(&self, key: &[u8]) -> RouteHint {
        let (shard, hash) = self.route_of(key);
        if shard == self.id {
            self.store
                .prefetch_for_hash(hash.unwrap_or_else(|| kevy_hash::KevyHash::kevy_hash(key)));
        }
        RouteHint { at: key.as_ptr() as usize, len: key.len(), shard, hash }
    }

    /// Start fetching the keyspace bucket a forwarded request will probe.
    #[inline]
    pub(crate) fn prefetch_request(&self, argv: &crate::Argv, meta: DispatchMeta) {
        match (meta.key_hash, meta.key_idx) {
            (Some(h), _) => self.store.prefetch_for_hash(h),
            (None, Some(i)) => {
                if let Some(key) = argv.get(usize::from(i)) {
                    self.store.prefetch_for_key(key);
                }
            }
            (None, None) => {}
        }
    }
}

/// The second argument of the RESP command at the front of `buf`, read
/// without parsing the rest; `None` when the frame is not all there or is
/// not a plain `*N` array of bulk strings with N ≥ 2. The parser that runs
/// the command reads the same bytes, so a key found here is the key it
/// sees.
#[inline]
fn peek_key(buf: &[u8]) -> Option<&[u8]> {
    let (n, mut at) = header(buf, b'*')?;
    if n < 2 {
        return None;
    }
    for i in 0..2 {
        let (len, body) = header(buf.get(at..)?, b'$')?;
        let start = at + body;
        let end = start.checked_add(len)?;
        if buf.get(end..end + 2)? != b"\r\n" {
            return None;
        }
        if i == 1 {
            return Some(&buf[start..end]);
        }
        at = end + 2;
    }
    None
}

/// `<tag><digits>\r\n` at the front of `buf`: the number and the length of
/// the header.
#[inline(always)]
fn header(buf: &[u8], tag: u8) -> Option<(usize, usize)> {
    if *buf.first()? != tag {
        return None;
    }
    let mut n = 0usize;
    let mut i = 1;
    while let Some(&d) = buf.get(i) {
        match d {
            b'0'..=b'9' if i < 12 => n = n * 10 + usize::from(d - b'0'),
            b'\r' if i > 1 => return (buf.get(i + 1) == Some(&b'\n')).then_some((n, i + 2)),
            _ => return None,
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::peek_key;

    #[test]
    fn the_key_is_argv_one_of_a_whole_frame() {
        assert_eq!(peek_key(b"*2\r\n$3\r\nGET\r\n$4\r\nk:12\r\n"), Some(&b"k:12"[..]));
        assert_eq!(peek_key(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv"), Some(&b"k"[..]));
        assert_eq!(peek_key(b"*3\r\n$3\r\nSET\r\n$0\r\n\r\n"), Some(&b""[..]));
    }

    #[test]
    fn anything_else_has_no_key() {
        let cases: [&[u8]; 9] = [
            b"",
            b"*1\r\n$4\r\nPING\r\n",
            b"*2\r\n$3\r\nGET\r\n$4\r\nk:1",
            b"*2\r\n$3\r\nGET\r\n$4\r\nk:12xx",
            b"*2\r\n$3\r\nGET\r\n:4\r\n",
            b"GET k\r\n",
            b"*\r\n",
            b"*2\r\n$99999999999999\r\n",
            b"*2\r\n$-1\r\n",
        ];
        for c in cases {
            assert_eq!(peek_key(c), None, "{:?}", String::from_utf8_lossy(c));
        }
    }
}
