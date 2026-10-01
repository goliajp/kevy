//! Shared-port connections go to the shard holding the fewest.
//!
//! `SO_REUSEPORT` picks a listener by hashing each connection's 4-tuple, so
//! the split depends on the clients' ephemeral ports: a pool of 48
//! connections over four shards lands anywhere from even to two to one,
//! differently on every run, and on macOS every connection lands on one
//! shard. The busiest shard then sets the server's throughput. So whichever
//! shard the kernel hands a connection to, it belongs to the shard with the
//! fewest live client connections — ties broken in a turn shared by all of
//! them — and is passed there if that is another shard. Counting live
//! connections rather than taking turns keeps the split even as clients
//! come and go. The count is taken when the shard is chosen, not when it
//! installs the connection, so a burst accepted on one shard spreads out
//! instead of piling onto whichever shard looked emptiest before any of it
//! landed. A passed connection costs one message at accept time and nothing
//! afterwards. Cluster ports are per shard and are never passed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kevy_sys::Socket;

use crate::message::Inbound;
use crate::shard::Shard;

/// What the accepting shards share: each shard's live client connections
/// and the turn that breaks ties.
pub(crate) struct AcceptShared {
    live: Box<[AtomicUsize]>,
    turn: AtomicUsize,
}

impl AcceptShared {
    /// For `width` accepting shards.
    pub(crate) fn new(width: usize) -> Arc<AcceptShared> {
        Arc::new(AcceptShared {
            live: (0..width).map(|_| AtomicUsize::new(0)).collect(),
            turn: AtomicUsize::new(0),
        })
    }
}

/// One shard's part: the shared counts, and the connections passed here
/// that the reactor has yet to install.
pub(crate) struct AcceptBalance {
    shared: Option<Arc<AcceptShared>>,
    adopted: Vec<(Socket, bool)>,
}

impl AcceptBalance {
    /// One shard needs no balancing.
    pub(crate) fn new(shared: &Arc<AcceptShared>) -> AcceptBalance {
        let shared = (shared.live.len() > 1).then(|| Arc::clone(shared));
        AcceptBalance { shared, adopted: Vec::new() }
    }

    /// A connection another shard passed here.
    pub(crate) fn adopt(&mut self, sock: Socket, unix: bool) {
        self.adopted.push((sock, unix));
    }

    /// Whether any passed connection waits; one load on the reactor loop.
    #[inline]
    pub(crate) fn has_adopted(&self) -> bool {
        !self.adopted.is_empty()
    }

    /// The passed connections waiting for the reactor to install them.
    pub(crate) fn take_adopted(&mut self) -> Vec<(Socket, bool)> {
        std::mem::take(&mut self.adopted)
    }

    /// The shard with the fewest live connections, counted for it now.
    fn choose(&self) -> Option<usize> {
        let sh = self.shared.as_ref()?;
        let w = sh.live.len();
        let start = sh.turn.fetch_add(1, Ordering::Relaxed) % w;
        let mut best = start;
        for i in 1..w {
            let s = (start + i) % w;
            if sh.live[s].load(Ordering::Relaxed) < sh.live[best].load(Ordering::Relaxed) {
                best = s;
            }
        }
        sh.live[best].fetch_add(1, Ordering::Relaxed);
        Some(best)
    }

    /// A client connection on `shard` closed, or was refused after it was
    /// counted.
    pub(crate) fn left(&self, shard: usize) {
        if let Some(sh) = &self.shared
            && let Some(n) = sh.live.get(shard)
        {
            n.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl<C: crate::Commands> Shard<C> {
    /// The connection `sock` from the shared port, if this shard has the
    /// fewest; otherwise it is passed to the shard that has.
    pub(crate) fn keep_or_pass(&mut self, sock: Socket, unix: bool) -> Option<Socket> {
        match self.balance.choose() {
            Some(dst) if dst != self.id => {
                self.send_to(dst, Inbound::Adopt { sock, unix });
                None
            }
            _ => Some(sock),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_on_one_shard_spreads_evenly() {
        let shared = AcceptShared::new(4);
        let b = AcceptBalance::new(&shared);
        let mut seen = [0usize; 4];
        for _ in 0..48 {
            seen[b.choose().unwrap()] += 1;
        }
        assert_eq!(seen, [12, 12, 12, 12]);
    }

    #[test]
    fn closed_connections_are_refilled_first() {
        let shared = AcceptShared::new(4);
        let b = AcceptBalance::new(&shared);
        for _ in 0..8 {
            b.choose();
        }
        // two clients on shard 2 hang up: the next two connections go there
        b.left(2);
        b.left(2);
        assert_eq!((b.choose(), b.choose()), (Some(2), Some(2)));
    }

    #[test]
    fn one_accepting_shard_keeps_everything() {
        let shared = AcceptShared::new(1);
        let only = AcceptBalance::new(&shared);
        assert_eq!(only.choose(), None);
    }
}
