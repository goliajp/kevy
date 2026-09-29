//! What a claim did, gathered for [`Effect::RecordClaim`]: the entries it
//! took, the pending entries it dropped, and whether it created its
//! consumer. The frames themselves are built only by a caller that
//! records (`crate::aof::deferred_frames`).

use kevy_resp::ArgvView;
use kevy_store::{Store, StreamId};

use crate::Effect;
use crate::aof::{Claim, Consumer};

/// The part of a group's state a claim's record depends on, read before
/// the claim runs.
pub(super) struct Before {
    consumer_existed: bool,
    /// Of the IDs an `XCLAIM` names, those pending at the time.
    pending: Vec<StreamId>,
}

impl Before {
    /// `args` is `XCLAIM|XAUTOCLAIM key group consumer …`; `ids` the IDs
    /// an `XCLAIM` names (none for `XAUTOCLAIM`).
    pub(super) fn read<A: ArgvView + ?Sized>(store: &Store, args: &A, ids: &[StreamId]) -> Before {
        let Some(g) = store.stream_group_peek(&args[1], &args[2]) else {
            return Before { consumer_existed: false, pending: Vec::new() };
        };
        Before {
            consumer_existed: g.consumer(&args[3]).is_some(),
            pending: ids.iter().copied().filter(|id| g.pending_entry(*id).is_some()).collect(),
        }
    }

    /// Of the IDs that were pending, those an `XCLAIM` dropped: not taken
    /// and not pending any more.
    pub(super) fn dropped(
        &self,
        store: &Store,
        key: &[u8],
        group: &[u8],
        taken: &[StreamId],
    ) -> Vec<StreamId> {
        let still = |id: &StreamId| {
            store.stream_group_peek(key, group).is_some_and(|g| g.pending_entry(*id).is_some())
        };
        self.pending.iter().copied().filter(|id| !taken.contains(id) && !still(id)).collect()
    }
}

/// What an `XREADGROUP` needs to know about each stream it reads, taken
/// before the read: the group's last-delivered ID and whether the consumer
/// is new. One stream is kept inline, so the common read notes its marks
/// without a heap allocation.
#[derive(Default)]
pub(super) struct ReadMarks {
    first: Option<(StreamId, Consumer)>,
    more: Vec<(StreamId, Consumer)>,
    changed: bool,
}

impl ReadMarks {
    /// The mark for one stream, read without side effects.
    pub(super) fn read(
        store: &Store,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
    ) -> (StreamId, Consumer) {
        match store.stream_group_peek(key, group) {
            Some(g) if g.consumer(consumer).is_none() => (g.last_delivered_id(), Consumer::Created),
            Some(g) => (g.last_delivered_id(), Consumer::Existing),
            None => (StreamId::MIN, Consumer::Existing),
        }
    }

    /// Note a stream's mark and whether the read delivered from it.
    pub(super) fn push(&mut self, mark: (StreamId, Consumer), delivered: bool) {
        self.changed |= delivered || mark.1 == Consumer::Created;
        match self.first {
            None => self.first = Some(mark),
            Some(_) => self.more.push(mark),
        }
    }

    /// The effect of the read: nothing to record when it delivered
    /// nothing and created no consumer. Its contact with the group is
    /// then not recorded either: a consumer that only polls comes back
    /// from a restart with the contact of its last recorded read.
    pub(super) fn effect(self) -> Effect {
        let Some(first) = self.first.filter(|_| self.changed) else { return Effect::Skip };
        if self.more.is_empty() {
            return Effect::RecordRead(first.0, first.1);
        }
        let mut all = Vec::with_capacity(1 + self.more.len());
        all.push(first);
        all.extend(self.more);
        Effect::RecordReads(all)
    }
}

/// The effect of a claim that took `taken` and dropped `dropped`:
/// nothing to record when it changed nothing.
pub(super) fn claim_effect(
    before: &Before,
    taken: Vec<StreamId>,
    dropped: Vec<StreamId>,
) -> Effect {
    let consumer = if before.consumer_existed { Consumer::Existing } else { Consumer::Created };
    let claim = Claim::new(taken, dropped, consumer);
    if claim.is_empty() { Effect::Skip } else { Effect::RecordClaim(Box::new(claim)) }
}
