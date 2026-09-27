//! The records a caller builds only when it records: a write whose argv
//! does not pin down what it did says so in its [`Effect`], cheaply, and
//! [`deferred_frames`] turns that into frames on demand. A caller with
//! nowhere to record a write never pays for its frames.
//!
//! * [`Effect::RecordId`] — an `XADD` whose ID was generated (`*`, `ms-*`):
//!   the argv with that one argument replaced by the ID it gave. The trim
//!   options stay and run again on replay, with the same result: a trim
//!   here is exact, `~` included, and depends only on the entries.
//! * [`Effect::RecordClaim`] — an `XCLAIM` / `XAUTOCLAIM`, which picks by
//!   idle time and stamps with the clock, recorded as its outcome in the
//!   form Redis documents for carrying a claim to the AOF and replicas:
//!   - each entry taken: `XCLAIM key group consumer 0 id… TIME t
//!     RETRYCOUNT n FORCE JUSTID`, with the delivery time and count the
//!     entry holds now; entries that share both share a frame. Minimum
//!     idle 0 takes them whatever the clock says, `TIME` and `RETRYCOUNT`
//!     restore the bookkeeping, `JUSTID` keeps the count from moving
//!     again, `FORCE` recreates a row the command itself forced.
//!   - each pending entry dropped because the stream no longer holds it:
//!     `XCLAIM key group consumer 0 id… JUSTID`, which drops it again.
//!   - nothing taken or dropped, but the consumer is new: `XGROUP
//!     CREATECONSUMER key group consumer`.
//!
//!   `LASTID` is not used: a claim here never moves the group's
//!   last-delivered ID, and the parser does not take the option.
//!
//! ```
//! use kevy_verbs::{Effect, aof::deferred_frames};
//! let mut store = kevy_store::Store::new();
//! let argv = kevy_resp::Argv::from(vec![b"XADD".to_vec(), b"s".to_vec(), b"*".to_vec(), b"f".to_vec(), b"v".to_vec()]);
//! let id = kevy_store::StreamId { ms: 5, seq: 1 };
//! let frames = deferred_frames(&mut store, &argv, &Effect::RecordId(2, id));
//! assert_eq!(&frames[0][2], b"5-1");
//! ```

use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, StreamId};

use crate::Effect;

/// What a claim did, for a caller that records it: see the module notes
/// for the frames it becomes.
///
/// ```
/// let c = kevy_verbs::aof::Claim::default();
/// assert!(c.is_empty(), "a claim that took, dropped and created nothing");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claim {
    taken: Vec<StreamId>,
    dropped: Vec<StreamId>,
    new_consumer: bool,
}

impl Claim {
    /// A claim that took `taken`, dropped `dropped` from the pending
    /// list, and created its consumer when `new_consumer`.
    ///
    /// ```
    /// let c = kevy_verbs::aof::Claim::new(Vec::new(), Vec::new(), true);
    /// assert!(!c.is_empty());
    /// ```
    pub fn new(taken: Vec<StreamId>, dropped: Vec<StreamId>, new_consumer: bool) -> Claim {
        Claim { taken, dropped, new_consumer }
    }

    /// Whether the claim changed nothing, so has nothing to record.
    ///
    /// ```
    /// assert!(kevy_verbs::aof::Claim::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.taken.is_empty() && self.dropped.is_empty() && !self.new_consumer
    }
}

/// `ms-seq` into `buf`, without a heap allocation: two `u64` in decimal
/// and a dash fit in 41 bytes.
///
/// ```
/// let mut buf = [0u8; 41];
/// let id = kevy_store::StreamId { ms: 1_790_000_000_000, seq: 12 };
/// assert_eq!(kevy_verbs::aof::id_bytes(&mut buf, id), b"1790000000000-12");
/// assert_eq!(kevy_verbs::aof::id_bytes(&mut buf, id), id.encode().as_slice());
/// ```
pub fn id_bytes(buf: &mut [u8; 41], id: StreamId) -> &[u8] {
    use std::io::Write;
    let mut w = &mut buf[..];
    // cannot fail: the buffer holds the longest pair
    let _ = write!(w, "{}-{}", id.ms, id.seq);
    let n = 41 - w.len();
    &buf[..n]
}

/// The frames to record for `effect`, the effect of `args` run against
/// `store` just now: one for [`Effect::RecordId`], one or more for
/// [`Effect::RecordClaim`], none for every other effect.
///
/// ```
/// let mut store = kevy_store::Store::new();
/// let argv = kevy_resp::Argv::from(vec![b"GET".to_vec(), b"k".to_vec()]);
/// assert!(kevy_verbs::aof::deferred_frames(&mut store, &argv, &kevy_verbs::Effect::Read).is_empty());
/// ```
pub fn deferred_frames<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    effect: &Effect,
) -> Vec<Argv> {
    match effect {
        Effect::RecordId(at, id) => {
            let encoded = id.encode();
            let mut f = Argv::with_capacity(args.len(), 0);
            for i in 0..args.len() {
                f.push(if i == *at { &encoded[..] } else { &args[i] });
            }
            vec![f]
        }
        Effect::RecordClaim(c) => claim_frames(store, args, c),
        _ => Vec::new(),
    }
}

fn claim_frames<A: ArgvView + ?Sized>(store: &mut Store, args: &A, c: &Claim) -> Vec<Argv> {
    let head = |ids: &[StreamId], tail: usize| {
        let mut f = Argv::with_capacity(5 + ids.len() + tail, 0);
        for part in [&b"XCLAIM"[..], &args[1], &args[2], &args[3], b"0"] {
            f.push(part);
        }
        for id in ids {
            f.push(&id.encode());
        }
        f
    };
    let mut frames = Vec::new();
    for ((time, count), ids) in bookkeeping(store, &args[1], &args[2], &c.taken) {
        let mut f = head(&ids, 6);
        f.push(b"TIME");
        f.push(time.to_string().as_bytes());
        f.push(b"RETRYCOUNT");
        f.push(count.to_string().as_bytes());
        f.push(b"FORCE");
        f.push(b"JUSTID");
        frames.push(f);
    }
    if !c.dropped.is_empty() {
        let mut f = head(&c.dropped, 1);
        f.push(b"JUSTID");
        frames.push(f);
    }
    if frames.is_empty() && c.new_consumer {
        let mut f = Argv::with_capacity(5, 0);
        for part in [&b"XGROUP"[..], b"CREATECONSUMER", &args[1], &args[2], &args[3]] {
            f.push(part);
        }
        frames.push(f);
    }
    frames
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
