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
//!   - last, the consumer's times as the claim left them (see
//!     [`Effect::RecordSeen`]): replayed, the frames before it stamp the
//!     consumer with the replay's clock, and this sets them back.
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

use kevy_resp::ops_table::{CONSUMER_SEEN, PENDING};
use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, StreamId};

use crate::Effect;

/// Whether a group read or claim found its consumer or created it. A
/// created consumer is recorded with the time it was made, so a replay
/// does not make it at its own.
///
/// ```
/// use kevy_verbs::aof::Consumer;
///
/// assert_ne!(Consumer::Existing, Consumer::Created);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Consumer {
    /// The consumer was already in the group.
    ///
    /// ```
    /// use kevy_verbs::{Effect, aof::Consumer, exec};
    /// if kevy_verbs::verb(b"XREADGROUP").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// for c in ["XADD s 1-1 f v", "XGROUP CREATE s g 0"] {
    ///     exec(&mut store, c.split(' ').next().unwrap_or("").as_bytes(), &argv(c), &mut Vec::new());
    /// }
    /// let read = argv("XREADGROUP GROUP g a STREAMS s >");
    /// exec(&mut store, b"XREADGROUP", &read, &mut Vec::new());
    /// exec(&mut store, b"XADD", &argv("XADD s 1-2 f v"), &mut Vec::new());
    /// // the second read by `a` finds the consumer the first one made
    /// let again = exec(&mut store, b"XREADGROUP", &read, &mut Vec::new());
    /// assert!(matches!(again, Some(Effect::RecordRead(_, Consumer::Existing))));
    /// ```
    Existing,
    /// The command created the consumer.
    ///
    /// ```
    /// use kevy_verbs::{Effect, aof::Consumer, exec};
    /// if kevy_verbs::verb(b"XREADGROUP").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// for c in ["XADD s 1-1 f v", "XGROUP CREATE s g 0"] {
    ///     exec(&mut store, c.split(' ').next().unwrap_or("").as_bytes(), &argv(c), &mut Vec::new());
    /// }
    /// let read = argv("XREADGROUP GROUP g a STREAMS s >");
    /// let first = exec(&mut store, b"XREADGROUP", &read, &mut Vec::new());
    /// assert!(matches!(first, Some(Effect::RecordRead(_, Consumer::Created))));
    /// ```
    Created,
}

/// What a group read that went over consumers' history did, for a caller
/// that records it: per stream, in `STREAMS` order, the group's
/// last-delivered ID before the read, whether the read created the
/// consumer, and the entries it delivered again.
///
/// ```
/// use kevy_verbs::aof::{Consumer, History};
/// let h = History::new(vec![(kevy_store::StreamId::MIN, Consumer::Existing)], vec![Vec::new()]);
/// assert_eq!(h, h.clone());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct History {
    marks: Vec<(StreamId, Consumer)>,
    redelivered: Vec<Vec<StreamId>>,
}

impl History {
    /// See [`History`]; `redelivered` may be shorter than `marks`.
    pub fn new(marks: Vec<(StreamId, Consumer)>, redelivered: Vec<Vec<StreamId>>) -> History {
        History { marks, redelivered }
    }
}

/// What a claim did, for a caller that records it: see the module notes
/// for the frames it becomes.
///
/// ```
/// let c = kevy_verbs::aof::Claim::default();
/// assert!(c.is_empty(), "a claim that took, dropped and created nothing");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Claim {
    taken: Vec<StreamId>,
    dropped: Vec<StreamId>,
    consumer: Consumer,
    /// `LASTID` moved the group's last-delivered ID.
    moved: bool,
}

impl Default for Claim {
    /// The claim that took and dropped nothing, by a consumer that
    /// already existed.
    fn default() -> Self {
        Claim::new(Vec::new(), Vec::new(), Consumer::Existing)
    }
}

impl Claim {
    /// A claim that took `taken`, dropped `dropped` from the pending
    /// list, by a `consumer` it found or created.
    ///
    /// ```
    /// use kevy_verbs::aof::{Claim, Consumer};
    ///
    /// let c = Claim::new(Vec::new(), Vec::new(), Consumer::Created);
    /// assert!(!c.is_empty());
    /// ```
    pub fn new(taken: Vec<StreamId>, dropped: Vec<StreamId>, consumer: Consumer) -> Claim {
        Claim { taken, dropped, consumer, moved: false }
    }

    /// The same claim, noting whether its `LASTID` moved the group's
    /// last-delivered ID.
    ///
    /// ```
    /// let c = kevy_verbs::aof::Claim::default().with_moved(true);
    /// assert!(!c.is_empty(), "moving the group is a change");
    /// ```
    #[must_use]
    pub fn with_moved(mut self, moved: bool) -> Claim {
        self.moved = moved;
        self
    }

    /// Whether the claim changed nothing, so has nothing to record.
    ///
    /// ```
    /// assert!(kevy_verbs::aof::Claim::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.taken.is_empty()
            && self.dropped.is_empty()
            && self.consumer == Consumer::Existing
            && !self.moved
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
        Effect::RecordAdd(at, id, kept) => vec![add_frame(args, *at, *id, *kept)],
        Effect::RecordClaim(c) => claim_frames(store, args, c),
        Effect::RecordSeen => seen_frame(store, &args[2], &args[3], &args[4]).into_iter().collect(),
        Effect::RecordRead(prev, consumer) => {
            crate::record_read::read_frames(store, args, &[(*prev, *consumer)], &[])
        }
        Effect::RecordReads(marks) => crate::record_read::read_frames(store, args, marks, &[]),
        Effect::RecordHistory(h) => {
            crate::record_read::read_frames(store, args, &h.marks, &h.redelivered)
        }
        Effect::Read | Effect::Write | Effect::Unchanged | Effect::Record(_) | Effect::Skip => {
            Vec::new()
        }
    }
}

/// `XADD key [NOMKSTREAM] [MAXLEN = kept] id field value…`.
fn add_frame<A: ArgvView + ?Sized>(args: &A, at: usize, id: StreamId, kept: u64) -> Argv {
    let refuse = (2..at).any(|i| args[i].eq_ignore_ascii_case(b"NOMKSTREAM"));
    let mut f = Argv::with_capacity(args.len() + 3, 0);
    f.push(&args[0]);
    f.push(&args[1]);
    if refuse {
        f.push(b"NOMKSTREAM");
    }
    if kept != u64::MAX {
        f.push(b"MAXLEN");
        f.push(b"=");
        f.push(kept.to_string().as_bytes());
    }
    f.push(id_bytes(&mut [0u8; 41], id));
    for i in at + 1..args.len() {
        f.push(&args[i]);
    }
    f
}

fn claim_frames<A: ArgvView + ?Sized>(store: &Store, args: &A, c: &Claim) -> Vec<Argv> {
    let (key, group, consumer) = (&args[1], &args[2], &args[3]);
    let mut frames: Vec<Argv> =
        c.moved.then(|| setid_frame(store, key, group)).flatten().into_iter().collect();
    frames.extend(taken_frames(store, key, group, consumer, &c.taken));
    if !c.dropped.is_empty() {
        let mut f = claim_head(key, group, consumer, &c.dropped, 1);
        f.push(b"JUSTID");
        frames.push(f);
    }
    frames.extend(seen_frame(store, key, group, consumer));
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

/// `XGROUP SETID key group <last-delivered> ENTRIESREAD <n|-1>`: where the
/// group stands now. `None` when the group is gone.
pub(crate) fn setid_frame(store: &Store, key: &[u8], group: &[u8]) -> Option<Argv> {
    let g = store.stream_group_peek(key, group)?;
    let read = g.entries_read().map_or_else(|| "-1".to_owned(), |n| n.to_string());
    let mut f = Argv::with_capacity(7, 0);
    for part in
        [&b"XGROUP"[..], b"SETID", key, group, &g.last_delivered_id().encode(), b"ENTRIESREAD"]
    {
        f.push(part);
    }
    f.push(read.as_bytes());
    Some(f)
}

/// `XINTERNAL.CONSUMERSEEN key group consumer t [a]`, `t` the consumer's
/// last contact with the group as it stands now and `a` the last time it
/// was handed an entry, absent if it never was: replayed, the consumer
/// exists with those times, whatever the replay's clock says. `None` when
/// the group or the consumer is gone.
pub(crate) fn seen_frame(store: &Store, key: &[u8], group: &[u8], consumer: &[u8]) -> Option<Argv> {
    let c = store.stream_group_peek(key, group)?.consumer(consumer)?;
    let mut f = Argv::with_capacity(6, 0);
    for part in [CONSUMER_SEEN.as_bytes(), key, group, consumer] {
        f.push(part);
    }
    f.push(c.last_seen_ms().to_string().as_bytes());
    if let Some(active) = c.last_active_ms() {
        f.push(active.to_string().as_bytes());
    }
    Some(f)
}

/// The refusal a client gets for sending an internal record verb.
///
/// ```
/// assert!(kevy_verbs::aof::INTERNAL_REFUSAL.starts_with("ERR "));
/// ```
pub const INTERNAL_REFUSAL: &str = "ERR the XINTERNAL verbs are written by kevy to its own records and are not accepted from a client";

/// Apply an internal record frame, one kevy writes and no client may send
/// (see [`kevy_resp::ops_table::CONSUMER_SEEN`] and
/// [`kevy_resp::ops_table::PENDING`]), appending its reply to `out`. `false` = `args` is not an internal record frame; `out` is
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
    let Some(verb) = args.get(0) else {
        return false;
    };
    if verb.eq_ignore_ascii_case(PENDING.as_bytes()) {
        apply_pending(store, args, out);
        return true;
    }
    if !verb.eq_ignore_ascii_case(CONSUMER_SEEN.as_bytes()) {
        return false;
    }
    let time = |i: usize| crate::args::arg_u64(&args[i]);
    let (seen, active) = match args.len() {
        5 => (time(4), None),
        6 => (time(4), time(5).map(Some)),
        _ => (None, None),
    };
    let (Some(seen), true) = (seen, args.len() == 5 || active.is_some()) else {
        kevy_resp::encode_error(out, "ERR malformed internal consumer record");
        return true;
    };
    let (key, group, consumer) = (&args[1], &args[2], &args[3]);
    let made = store.xgroup_consumer_seen(key, group, consumer, seen);
    let made = match (made, active) {
        (Ok(made), Some(at)) => {
            store.xgroup_consumer_active(key, group, consumer, at).map(|_| made)
        }
        (made, _) => made,
    };
    match made {
        Ok(made) => kevy_resp::encode_integer(out, i64::from(made)),
        Err(e) => crate::reply::store_err(out, e),
    }
    true
}

/// `XINTERNAL.PENDING key group consumer delivery-ms delivery-count id`.
fn apply_pending<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    let n = |i: usize| args.get(i).and_then(crate::args::arg_u64);
    let id = args.get(6).and_then(|id| kevy_store::parse_explicit_id(id).ok());
    let (7, Some(at), Some(count), Some(id)) = (args.len(), n(4), n(5), id) else {
        kevy_resp::encode_error(out, "ERR malformed internal pending record");
        return;
    };
    match store.xgroup_restore_pending(&args[1], &args[2], &args[3], id, at, count) {
        Ok(put) => kevy_resp::encode_integer(out, i64::from(put)),
        Err(e) => crate::reply::store_err(out, e),
    }
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
    let mut by: Vec<((u64, u64), Vec<StreamId>)> = Vec::new();
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
