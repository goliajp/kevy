//! What a claim is recorded as.
//!
//! `XCLAIM` and `XAUTOCLAIM` pick what they take by comparing each pending
//! entry's idle time with `min-idle-time`, and stamp what they take with
//! the clock. Replayed as typed, at another time, they would take other
//! entries or none. So a claim is recorded as its outcome, in the form
//! Redis documents for carrying a claim to the AOF and to replicas:
//!
//! * each entry taken: `XCLAIM key group consumer 0 id… TIME t RETRYCOUNT
//!   n FORCE JUSTID`, with the delivery time and count the entry holds
//!   now; entries that share both share a frame. Minimum idle 0 takes
//!   them whatever the clock says, `TIME` and `RETRYCOUNT` restore the
//!   bookkeeping, `JUSTID` keeps the count from moving again, `FORCE`
//!   recreates a row the command itself forced.
//! * each pending entry dropped because the stream no longer holds it:
//!   `XCLAIM key group consumer 0 id… JUSTID`, which drops it again.
//! * nothing taken or dropped, but the consumer is new: `XGROUP
//!   CREATECONSUMER key group consumer`.
//! * nothing changed: no record.
//!
//! `LASTID` is not used: a claim here never moves the group's
//! last-delivered ID, and the parser does not take the option.

use kevy_resp::ArgvView;
use kevy_store::{Store, StreamId};

use crate::Effect;

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

/// The record of a claim that took `taken` and dropped `dropped`.
pub(super) fn claim_effect<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    before: &Before,
    taken: &[StreamId],
    dropped: &[StreamId],
) -> Effect {
    let head = |ids: &[StreamId]| {
        let mut f: Vec<Vec<u8>> = vec![b"XCLAIM".to_vec(), args[1].to_vec(), args[2].to_vec()];
        f.push(args[3].to_vec());
        f.push(b"0".to_vec());
        f.extend(ids.iter().map(|id| id.encode()));
        f
    };
    let mut frames: Vec<Vec<Vec<u8>>> = Vec::new();
    for ((time, count), ids) in bookkeeping(store, &args[1], &args[2], taken) {
        let mut f = head(&ids);
        f.extend([b"TIME".to_vec(), time.to_string().into_bytes()]);
        f.extend([b"RETRYCOUNT".to_vec(), count.to_string().into_bytes()]);
        f.extend([b"FORCE".to_vec(), b"JUSTID".to_vec()]);
        frames.push(f);
    }
    if !dropped.is_empty() {
        let mut f = head(dropped);
        f.push(b"JUSTID".to_vec());
        frames.push(f);
    }
    if frames.is_empty() && !before.consumer_existed {
        let create: [&[u8]; 5] = [b"XGROUP", b"CREATECONSUMER", &args[1], &args[2], &args[3]];
        frames.push(create.iter().map(|p| p.to_vec()).collect());
    }
    if frames.is_empty() { Effect::Skip } else { Effect::RecordAll(frames) }
}

/// The taken IDs grouped by the `(delivery time, delivery count)` their
/// pending rows hold now, in the order the groups first appear.
fn bookkeeping(
    store: &mut Store,
    key: &[u8],
    group: &[u8],
    taken: &[StreamId],
) -> Vec<((u64, u32), Vec<StreamId>)> {
    let Ok(Some(s)) = store.stream_view(key) else { return Vec::new() };
    let Some(g) = s.group(group) else { return Vec::new() };
    let mut out: Vec<((u64, u32), Vec<StreamId>)> = Vec::new();
    for id in taken {
        let Some(row) = g.pel.get(id) else { continue };
        let at = (row.delivery_time_ms, row.delivery_count);
        match out.iter_mut().find(|(k, _)| *k == at) {
            Some((_, ids)) => ids.push(*id),
            None => out.push((at, vec![*id])),
        }
    }
    out
}
