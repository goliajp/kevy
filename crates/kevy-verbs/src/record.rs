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
//! let id = kevy_store::StreamId::new(5, 1);
//! let frames = deferred_frames(&mut store, &argv, &Effect::RecordId(2, id));
//! assert_eq!(&frames[0][2], b"5-1");
//! ```

use kevy_resp::ops_table::CONSUMER_SEEN;
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
/// let id = kevy_store::StreamId::new(1_790_000_000_000, 12);
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
/// [`Effect::RecordClaim`], [`Effect::RecordRead`] and
/// [`Effect::RecordReads`], one for [`Effect::RecordSeen`], none for every
/// other effect. It only reads
/// `store`, and with no side effects: recording a write never changes
/// what the write left (a stream is never spilled to the cold tier, so
/// the groups it reads are resident).
///
/// ```
/// let store = kevy_store::Store::new();
/// let argv = kevy_resp::Argv::from(vec![b"GET".to_vec(), b"k".to_vec()]);
/// assert!(kevy_verbs::aof::deferred_frames(&store, &argv, &kevy_verbs::Effect::Read).is_empty());
/// ```
pub fn deferred_frames<A: ArgvView + ?Sized>(
    store: &Store,
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
        Effect::RecordSeen => seen_frame(store, &args[2], &args[3], &args[4]).into_iter().collect(),
        Effect::RecordRead(prev, made) => {
            crate::record_read::read_frames(store, args, &[(*prev, *made)])
        }
        Effect::RecordReads(marks) => crate::record_read::read_frames(store, args, marks),
        _ => Vec::new(),
    }
}

fn claim_frames<A: ArgvView + ?Sized>(store: &Store, args: &A, c: &Claim) -> Vec<Argv> {
    let (key, group, consumer) = (&args[1], &args[2], &args[3]);
    let mut frames = taken_frames(store, key, group, consumer, &c.taken);
    if !c.dropped.is_empty() {
        let mut f = claim_head(key, group, consumer, &c.dropped, 1);
        f.push(b"JUSTID");
        frames.push(f);
    }
    // first, so the XCLAIM frames find the consumer and leave its time be
    if c.new_consumer {
        frames.splice(0..0, seen_frame(store, key, group, consumer));
    }
    frames
}

/// `XCLAIM key group consumer 0 id…`, with room for `tail` more parts.
pub(crate) fn claim_head(
    key: &[u8],
    group: &[u8],
    consumer: &[u8],
    ids: &[StreamId],
    tail: usize,
) -> Argv {
    let mut f = Argv::with_capacity(5 + ids.len() + tail, 0);
    for part in [&b"XCLAIM"[..], key, group, consumer, b"0"] {
        f.push(part);
    }
    for id in ids {
        f.push(&id.encode());
    }
    f
}

/// `XINTERNAL.CONSUMERSEEN key group consumer t`, `t` the consumer's last
/// contact with the group as it stands now: replayed, the consumer exists
/// with that time, whatever the replay's clock says. `None` when the group
/// or the consumer is gone.
pub(crate) fn seen_frame(store: &Store, key: &[u8], group: &[u8], consumer: &[u8]) -> Option<Argv> {
    let seen = store.stream_group_peek(key, group)?.consumer(consumer)?.last_seen_ms();
    let mut f = Argv::with_capacity(5, 0);
    for part in [CONSUMER_SEEN.as_bytes(), key, group, consumer] {
        f.push(part);
    }
    f.push(seen.to_string().as_bytes());
    Some(f)
}

/// The refusal a client gets for sending an internal record verb.
///
/// ```
/// assert!(kevy_verbs::aof::INTERNAL_REFUSAL.starts_with("ERR "));
/// ```
pub const INTERNAL_REFUSAL: &str = "ERR XINTERNAL.CONSUMERSEEN is written by kevy to its own records and is not accepted from a client";

/// Apply an internal record frame, one kevy writes and no client may send
/// (see [`kevy_resp::ops_table::CONSUMER_SEEN`]), appending its reply to
/// `out`. `false` = `args` is not an internal record frame; `out` is
/// untouched. Only a caller applying a record — a replay, a replica —
/// calls this; a client's command goes through [`crate::exec`], which does
/// not answer these verbs.
///
/// ```
/// if kevy_verbs::verb(b"XGROUP").is_none() {
///     return; // built without the `streams-geo` feature
/// }
/// let mut store = kevy_store::Store::new();
/// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// let mut out = Vec::new();
/// kevy_verbs::exec(&mut store, b"XGROUP", &argv("XGROUP CREATE s g $ MKSTREAM"), &mut out);
/// out.clear();
/// assert!(kevy_verbs::aof::apply_internal(&mut store, &argv("XINTERNAL.CONSUMERSEEN s g c 40"), &mut out));
/// assert_eq!(out, b":1\r\n", "the consumer was made, seen at 40");
/// assert!(!kevy_verbs::aof::apply_internal(&mut store, &argv("GET s"), &mut out));
/// ```
pub fn apply_internal<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    if !args.get(0).is_some_and(|v| v.eq_ignore_ascii_case(CONSUMER_SEEN.as_bytes())) {
        return false;
    }
    let Some(seen) = (args.len() == 5).then(|| crate::args::arg_u64(&args[4])).flatten() else {
        kevy_resp::encode_error(out, "ERR malformed internal consumer record");
        return true;
    };
    match store.xgroup_consumer_seen(&args[1], &args[2], &args[3], seen) {
        Ok(made) => kevy_resp::encode_integer(out, i64::from(made)),
        Err(e) => crate::reply::store_err(out, e),
    }
    true
}

/// One `XCLAIM … TIME t RETRYCOUNT n FORCE JUSTID` per `(delivery time,
/// delivery count)` the pending rows of `ids` hold now, in the order those
/// pairs first appear.
pub(crate) fn taken_frames(
    store: &Store,
    key: &[u8],
    group: &[u8],
    consumer: &[u8],
    ids: &[StreamId],
) -> Vec<Argv> {
    let Some(g) = store.stream_group_peek(key, group) else { return Vec::new() };
    let mut by: Vec<((u64, u32), Vec<StreamId>)> = Vec::new();
    for id in ids {
        let Some(row) = g.pending_entry(*id) else { continue };
        let at = (row.delivery_time_ms, row.delivery_count);
        match by.iter_mut().find(|(k, _)| *k == at) {
            Some((_, same)) => same.push(*id),
            None => by.push((at, vec![*id])),
        }
    }
    by.into_iter()
        .map(|((time, count), ids)| {
            let mut f = claim_head(key, group, consumer, &ids, 6);
            f.push(b"TIME");
            f.push(time.to_string().as_bytes());
            f.push(b"RETRYCOUNT");
            f.push(count.to_string().as_bytes());
            f.push(b"FORCE");
            f.push(b"JUSTID");
            f
        })
        .collect()
}
