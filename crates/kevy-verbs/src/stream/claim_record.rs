//! What a claim did, gathered for [`Effect::RecordClaim`]: the entries it
//! took, the pending entries it dropped, and whether it created its
//! consumer. The frames themselves are built only by a caller that
//! records (`crate::aof::deferred_frames`).

use kevy_resp::ArgvView;
use kevy_store::{Store, StreamId};

use crate::Effect;
use crate::aof::Claim;

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
    pub(super) fn read<A: ArgvView + ?Sized>(
        store: &mut Store,
        args: &A,
        ids: &[StreamId],
    ) -> Before {
        let group = match store.stream_view(&args[1]) {
            Ok(Some(s)) => s.group(&args[2]),
            _ => None,
        };
        let Some(g) = group else {
            return Before { consumer_existed: false, pending: Vec::new() };
        };
        Before {
            consumer_existed: g.consumers.get(&args[3]).is_some(),
            pending: ids.iter().copied().filter(|id| g.pel.contains_key(id)).collect(),
        }
    }

    /// Of the IDs that were pending, those an `XCLAIM` dropped: not taken
    /// and not pending any more.
    pub(super) fn dropped(
        &self,
        store: &mut Store,
        key: &[u8],
        group: &[u8],
        taken: &[StreamId],
    ) -> Vec<StreamId> {
        let still = |store: &mut Store, id: &StreamId| match store.stream_view(key) {
            Ok(Some(s)) => s.group(group).is_some_and(|g| g.pel.contains_key(id)),
            _ => false,
        };
        self.pending.iter().copied().filter(|id| !taken.contains(id) && !still(store, id)).collect()
    }
}

/// The effect of a claim that took `taken` and dropped `dropped`:
/// nothing to record when it changed nothing.
pub(super) fn claim_effect(
    before: &Before,
    taken: Vec<StreamId>,
    dropped: Vec<StreamId>,
) -> Effect {
    let claim = Claim::new(taken, dropped, !before.consumer_existed);
    if claim.is_empty() { Effect::Skip } else { Effect::RecordClaim(Box::new(claim)) }
}
