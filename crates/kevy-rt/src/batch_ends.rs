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
        // a held conn's input waits whole (see `crate::exec_hold`)
        if self.is_held(conn_id) {
            return BatchOutcome { consumed: 0, protocol_error: false, conn_gone: false };
        }
        loop {
            match parse_command_borrowed(&buf[off..]) {
                Ok(Some((argv, consumed))) => {
                    if let Some(key) = argv.get(1) {
                        // slot routing hashes for the slot, not the bucket,
                        // so routing first would only delay the prefetch
                        if self.nshards == 1 || self.cluster.is_some() {
                            self.store.prefetch_for_key(key);
                        } else {
                            self.route_hint = Some(self.prefetch_local(key));
                        }
                    }
                    self.handle_command(conn_id, &argv);
                    self.route_hint = None;
                    drop(argv);
                    off += consumed;
                    match crate::conn::conn_at(&mut self.conns, &mut self.conn_slot_hint, conn_id) {
                        None => {
                            return BatchOutcome {
                                consumed: off,
                                protocol_error: false,
                                conn_gone: true,
                            };
                        }
                        Some(c) if c.hold.is_some() => {
                            return BatchOutcome {
                                consumed: off,
                                protocol_error: false,
                                conn_gone: false,
                            };
                        }
                        Some(_) => {}
                    }
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
            (0, Some(i)) => {
                if let Some(key) = argv.get(usize::from(i)) {
                    self.store.prefetch_for_key(key);
                }
            }
            (0, None) => {}
            (h, _) => self.store.prefetch_for_hash(h),
        }
    }
}
