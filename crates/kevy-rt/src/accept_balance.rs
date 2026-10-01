//! Shared-port connections spread over the accepting shards in turn.
//!
//! `SO_REUSEPORT` picks a listener by hashing each connection's 4-tuple, so
//! the split depends on the clients' ephemeral ports: a pool of 48
//! connections over four shards lands anywhere from even to two to one,
//! and differently on every run. The busiest shard then sets the server's
//! throughput, and a measurement of the same build moves from run to run
//! with the split. So whichever shard the kernel hands a connection to,
//! the connection belongs to the next shard in a turn shared by all of
//! them, and is passed there if that is another shard. A passed
//! connection costs one message at accept time and nothing afterwards.
//! Cluster ports are per shard and are never passed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kevy_sys::Socket;

use crate::message::Inbound;
use crate::shard::Shard;

/// One shard's part of the turn: the counter all accepting shards share,
/// how many shards take connections, and the connections passed here that
/// the reactor has yet to install.
pub(crate) struct AcceptBalance {
    turn: Option<Arc<AtomicUsize>>,
    width: usize,
    adopted: Vec<(Socket, bool)>,
}

impl AcceptBalance {
    /// `width` shards take connections; one shard needs no turn.
    pub(crate) fn new(turn: &Arc<AtomicUsize>, width: usize) -> AcceptBalance {
        let turn = (width > 1).then(|| Arc::clone(turn));
        AcceptBalance { turn, width, adopted: Vec::new() }
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

    fn next(&self) -> Option<usize> {
        let turn = self.turn.as_ref()?;
        Some(turn.fetch_add(1, Ordering::Relaxed) % self.width)
    }
}

impl<C: crate::Commands> Shard<C> {
    /// The connection `sock` from the shared port, if it is this shard's
    /// turn; otherwise it is passed to the shard whose turn it is.
    pub(crate) fn keep_or_pass(&mut self, sock: Socket, unix: bool) -> Option<Socket> {
        match self.balance.next() {
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
    fn the_turn_visits_every_shard_once_per_round() {
        let turn = Arc::new(AtomicUsize::new(0));
        let shards: Vec<AcceptBalance> = (0..4).map(|_| AcceptBalance::new(&turn, 4)).collect();
        let mut seen = [0usize; 4];
        // whichever shard the kernel picks, the counts come out even
        for i in 0..48 {
            let dst = shards[(i * 7) % 4].next().unwrap();
            seen[dst] += 1;
        }
        assert_eq!(seen, [12, 12, 12, 12]);
    }

    #[test]
    fn one_accepting_shard_keeps_everything() {
        let turn = Arc::new(AtomicUsize::new(0));
        let only = AcceptBalance::new(&turn, 1);
        assert_eq!(only.next(), None);
        assert_eq!(turn.load(Ordering::Relaxed), 0);
    }
}
