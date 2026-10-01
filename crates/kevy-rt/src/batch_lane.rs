//! [`BatchLane`]: the origin shard's single-key lane to one peer, and the
//! envelope recycling that keeps a steady stream of batches from
//! allocating.

use crate::message::{Part, ReqBatch};
use kevy_resp::Argv;

/// An emptied envelope bigger than this many entries is dropped rather
/// than kept for reuse, so one burst does not pin its peak batch size.
const SPARE_MAX: usize = 1024;
/// The same cap for a reply envelope's byte buffer.
const SPARE_BYTES_MAX: usize = 1 << 20;

/// The replies `(conn, seq, part)` to one request batch, sent back as one
/// message. A plain reply is a [`Part::Spanned`] whose bytes follow the
/// previous one's in `bytes`, written there by the owner's dispatch, so
/// no reply is copied or allocated on its own. Each reply carries the
/// request's spent `Argv` husk back to the origin, which drops it into its
/// own [`kevy_resp::ArgvPool`] — so every shard's pool level matches its
/// own conn demand by construction, immune to accept skew (a conn-heavy
/// shard forwards more than it receives, so recycle-at-the-owner starves
/// its pool while overfilling quiet shards').
#[derive(Default)]
pub(crate) struct RespBatch {
    pub(crate) items: Vec<(u64, u64, Part, Argv)>,
    pub(crate) bytes: Vec<u8>,
}

/// The origin shard's single-key lane to one peer: the batch being filled,
/// and the envelopes that came back from that peer emptied. A request
/// batch goes out carrying an empty reply envelope and comes back as its
/// replies carrying the emptied request envelope, so a steady stream of
/// batches allocates nothing and frees nothing across threads.
#[derive(Default)]
pub(crate) struct BatchLane {
    pub(crate) reqs: ReqBatch,
    spare_reqs: Vec<ReqBatch>,
    spare_resps: Vec<RespBatch>,
    /// Scratch for the epoll reactor: the conns one reply batch from this
    /// peer touched, flushed once each.
    pub(crate) to_flush: Vec<u64>,
}

impl BatchLane {
    /// Take the filled batch, leaving a recycled empty one in its place,
    /// and an empty reply envelope for the peer to answer in.
    pub(crate) fn take(&mut self) -> (ReqBatch, RespBatch) {
        let next = self.spare_reqs.pop().unwrap_or_default();
        let reqs = std::mem::replace(&mut self.reqs, next);
        (reqs, self.spare_resps.pop().unwrap_or_default())
    }

    /// Keep the two envelopes a reply batch brought back, both emptied.
    pub(crate) fn recycle(&mut self, resps: RespBatch, reqs: ReqBatch) {
        if resps.items.capacity() <= SPARE_MAX && resps.bytes.capacity() <= SPARE_BYTES_MAX {
            self.spare_resps.push(resps);
        }
        if reqs.capacity() <= SPARE_MAX {
            self.spare_reqs.push(reqs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BatchLane, RespBatch, SPARE_BYTES_MAX, SPARE_MAX};

    #[test]
    fn a_round_trip_reuses_both_envelopes() {
        let mut lane = BatchLane::default();
        lane.reqs.reserve(8);
        let req_buf = lane.reqs.as_ptr();
        let (mut reqs, mut resps) = lane.take();
        assert_eq!(reqs.as_ptr(), req_buf, "the filled batch goes out as is");
        assert_eq!(resps.items.capacity(), 0, "no spare yet: the peer allocates the first");
        resps.items.reserve(8);
        let resp_buf = resps.items.as_ptr();
        reqs.clear();
        lane.recycle(resps, reqs);
        let (reqs, resps) = lane.take();
        assert_eq!(resps.items.as_ptr(), resp_buf, "the reply envelope goes back out");
        assert_eq!(lane.reqs.as_ptr(), req_buf, "the request envelope is refilled");
        assert!(reqs.is_empty() && lane.reqs.capacity() >= 8);
    }

    #[test]
    fn an_envelope_past_the_cap_is_not_kept() {
        let mut lane = BatchLane::default();
        let big = RespBatch { items: Vec::with_capacity(SPARE_MAX + 1), bytes: Vec::new() };
        lane.recycle(big, Vec::with_capacity(SPARE_MAX + 1));
        let (_, resps) = lane.take();
        assert_eq!(lane.reqs.capacity(), 0);
        assert_eq!(resps.items.capacity(), 0);
        let big = RespBatch { items: Vec::new(), bytes: Vec::with_capacity(SPARE_BYTES_MAX + 1) };
        lane.recycle(big, Vec::new());
        assert_eq!(lane.take().1.bytes.capacity(), 0, "a big byte buffer is not kept");
    }
}
