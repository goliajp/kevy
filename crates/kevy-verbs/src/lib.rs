//! The command layer shared by kevy's server and its embedded engine.
//!
//! [`exec`] runs one command against one `kevy_store::Store` and writes
//! its RESP reply: the argv grammar, the checks, the store call and the
//! reply wording all live here, once. What differs between the server
//! and the embedded engine — locking, routing keys to shards, multi-key
//! coordination, where a write is recorded — stays with each of them,
//! and [`Effect`] tells them what the command did so they can record it.
//!
//! [`args`] reads argv tokens the way Redis does and [`reply`] writes
//! replies with Redis's wording, for the commands that live outside
//! this crate.
//!
//! The stream (`X*`) and geo (`GEO*`) commands are behind the
//! `streams-geo` feature, off by default.
//!
//! ```
//! use kevy_verbs::{Effect, exec};
//!
//! let mut store = kevy_store::Store::new();
//! let argv = kevy_resp::Argv::from(vec![b"HSET".to_vec(), b"h".to_vec(), b"f".to_vec(), b"v".to_vec()]);
//! let mut out = Vec::new();
//! assert_eq!(exec(&mut store, b"HSET", &argv, &mut out), Some(Effect::Write));
//! assert_eq!(out, b":1\r\n");
//! ```

use kevy_resp::ArgvView;
use kevy_store::{Store, StreamId};

pub mod aof;
pub mod args;
mod bitmap;
pub mod cmd;
#[cfg(feature = "streams-geo")]
pub mod geo;
mod hash;
mod hash_ttl;
mod keyspace;
mod list;
mod list_move;
mod record;
mod record_read;
pub mod reply;
mod set;
#[cfg(feature = "streams-geo")]
mod stream;
mod strings;
mod verbs;
mod zset;
mod zset_range;

pub use verbs::{VERBS, Verb, is_streams_geo, is_write, verb};

/// What a command did, for a caller that records writes.
///
/// It describes a command whose reply was not an error. An error reply
/// always means nothing changed, whatever this says.
///
/// ```
/// use kevy_verbs::{Effect, exec};
///
/// let mut store = kevy_store::Store::new();
/// let del = kevy_resp::Argv::from(vec![b"DEL".to_vec(), b"missing".to_vec()]);
/// let mut out = Vec::new();
/// assert_eq!(exec(&mut store, b"DEL", &del, &mut out), Some(Effect::Unchanged));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// A read: nothing to record.
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// assert_eq!(exec(&mut store, b"GET", &argv("GET k"), &mut Vec::new()), Some(Effect::Read));
    /// ```
    Read,
    /// A write: record the argv as it was run.
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// assert_eq!(exec(&mut store, b"SET", &argv("SET k v"), &mut Vec::new()), Some(Effect::Write));
    /// ```
    Write,
    /// A write that changed nothing this time, such as `SET … NX` on a
    /// key that exists or `HDEL` of a missing field. Recording the argv
    /// is harmless; a caller that records only changes can skip it.
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// exec(&mut store, b"SET", &argv("SET k v"), &mut Vec::new());
    /// let nx = argv("SET k w NX");
    /// assert_eq!(exec(&mut store, b"SET", &nx, &mut Vec::new()), Some(Effect::Unchanged));
    /// ```
    Unchanged,
    /// Record this frame instead of the argv. A command whose effect is
    /// random (`SPOP`) is recorded as what it did (`SREM key member…`),
    /// so replaying the record cannot pick differently.
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// exec(&mut store, b"SADD", &argv("SADD s only"), &mut Vec::new());
    /// let effect = exec(&mut store, b"SPOP", &argv("SPOP s"), &mut Vec::new());
    /// let srem = ["SREM", "s", "only"].map(|w| w.as_bytes().to_vec()).to_vec();
    /// assert_eq!(effect, Some(Effect::Record(srem)));
    /// ```
    Record(Vec<Vec<u8>>),
    /// Record the argv with argument `.0` replaced by the ID `.1`: an `XADD`
    /// whose ID was generated (`*`, `ms-*`), recorded as the ID it gave so
    /// a replay's clock cannot pick another. Carries no frame: a caller
    /// that records builds it with [`aof::deferred_frames`].
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// if kevy_verbs::verb(b"XADD").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let mut store = kevy_store::Store::new();
    /// let argv = kevy_resp::Argv::from(vec![b"XADD".to_vec(), b"s".to_vec(), b"7-*".to_vec(), b"f".to_vec(), b"v".to_vec()]);
    /// let id = kevy_store::StreamId::new(7, 0);
    /// assert_eq!(exec(&mut store, b"XADD", &argv, &mut Vec::new()), Some(Effect::RecordId(2, id)));
    /// ```
    RecordId(usize, StreamId),
    /// Record a claim as its outcome: an `XCLAIM` / `XAUTOCLAIM`, which
    /// picks by idle time and stamps with the clock. Carries no frame: a
    /// caller that records builds them with [`aof::deferred_frames`].
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// if kevy_verbs::verb(b"XCLAIM").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let mut store = kevy_store::Store::new();
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// for c in ["XADD s 1-1 f v", "XGROUP CREATE s g 0", "XREADGROUP GROUP g a STREAMS s >"] {
    ///     let up = c.split(' ').next().unwrap().as_bytes();
    ///     exec(&mut store, up, &argv(c), &mut Vec::new());
    /// }
    /// let claim = argv("XCLAIM s g b 0 1-1 JUSTID");
    /// let effect = exec(&mut store, b"XCLAIM", &claim, &mut Vec::new()).unwrap();
    /// assert!(matches!(effect, Effect::RecordClaim(_)));
    /// let frames = kevy_verbs::aof::deferred_frames(&mut store, &claim, &effect);
    /// // the claim, then b's times as the claim left them
    /// let head: Vec<&[u8]> = (0..6).map(|i| &frames[0][i]).collect();
    /// assert_eq!(head, [&b"XCLAIM"[..], b"s", b"g", b"b", b"0", b"1-1"]);
    /// assert_eq!(&frames[1][0], b"XINTERNAL.CONSUMERSEEN");
    /// ```
    RecordClaim(Box<aof::Claim>),
    /// Record a one-stream `XREADGROUP` as what it left, not as a read a
    /// replay would stamp with its own clock: `.0` is the group's
    /// last-delivered ID before the read, `.1` whether the read created its
    /// consumer. Carries no frame: a caller that records builds them with
    /// [`aof::deferred_frames`].
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// if kevy_verbs::verb(b"XREADGROUP").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let mut store = kevy_store::Store::new();
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// for c in ["XADD s 1-1 f v", "XGROUP CREATE s g 0"] {
    ///     exec(&mut store, c.split(' ').next().unwrap().as_bytes(), &argv(c), &mut Vec::new());
    /// }
    /// let read = argv("XREADGROUP GROUP g a STREAMS s >");
    /// let effect = exec(&mut store, b"XREADGROUP", &read, &mut Vec::new()).unwrap();
    /// assert_eq!(effect, Effect::RecordRead(kevy_store::StreamId::MIN, kevy_verbs::aof::Consumer::Created));
    /// let frames = kevy_verbs::aof::deferred_frames(&store, &read, &effect);
    /// let verbs: Vec<&[u8]> = frames.iter().map(|f| &f[0]).collect();
    /// // the group's move, the delivery, then the consumer's times
    /// assert_eq!(verbs, [&b"XGROUP"[..], b"XCLAIM", b"XINTERNAL.CONSUMERSEEN"]);
    /// ```
    RecordRead(StreamId, aof::Consumer),
    /// [`Effect::RecordRead`] for an `XREADGROUP` over several streams:
    /// one `(last-delivered before, consumer created)` pair per stream, in
    /// `STREAMS` order.
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// if kevy_verbs::verb(b"XREADGROUP").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let mut store = kevy_store::Store::new();
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// for c in ["XADD a 1-1 f v", "XADD b 1-1 f v", "XGROUP CREATE a g 0", "XGROUP CREATE b g 0"] {
    ///     exec(&mut store, c.split(' ').next().unwrap().as_bytes(), &argv(c), &mut Vec::new());
    /// }
    /// let read = argv("XREADGROUP GROUP g c STREAMS a b > >");
    /// let Some(Effect::RecordReads(marks)) = exec(&mut store, b"XREADGROUP", &read, &mut Vec::new()) else {
    ///     panic!("a read of two streams is marked per stream")
    /// };
    /// assert_eq!(marks.len(), 2);
    /// ```
    RecordReads(Vec<(StreamId, aof::Consumer)>),
    /// Record an `XGROUP CREATECONSUMER` that created its consumer as
    /// `XINTERNAL.CONSUMERSEEN key group consumer t`, `t` the time it was
    /// created at, so a replay does not create it at its own. Carries no
    /// frame: a caller that records builds it with
    /// [`aof::deferred_frames`].
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// if kevy_verbs::verb(b"XGROUP").is_none() {
    ///     return; // built without the `streams-geo` feature
    /// }
    /// let mut store = kevy_store::Store::new();
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// exec(&mut store, b"XGROUP", &argv("XGROUP CREATE s g $ MKSTREAM"), &mut Vec::new());
    /// let create = argv("XGROUP CREATECONSUMER s g c");
    /// let effect = exec(&mut store, b"XGROUP", &create, &mut Vec::new()).unwrap();
    /// assert_eq!(effect, Effect::RecordSeen);
    /// let frame = &kevy_verbs::aof::deferred_frames(&store, &create, &effect)[0];
    /// assert_eq!((frame.len(), &frame[0]), (5, &b"XINTERNAL.CONSUMERSEEN"[..]));
    /// ```
    RecordSeen,
    /// Record nothing, not even the argv: a random command that removed
    /// nothing, or a claim that changed nothing.
    ///
    /// ```
    /// use kevy_verbs::{Effect, exec};
    /// let argv = |s: &str| kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
    /// let mut store = kevy_store::Store::new();
    /// // popping from a set that does not exist removes nothing
    /// assert_eq!(exec(&mut store, b"SPOP", &argv("SPOP none"), &mut Vec::new()), Some(Effect::Skip));
    /// ```
    Skip,
}

/// Run one command against `store`, appending its RESP2 reply to `out`.
///
/// `verb` is `args[0]` uppercased (see [`args::upper_verb`]). `None`
/// means the verb is not one this layer executes; `out` is untouched.
///
/// ```
/// let mut store = kevy_store::Store::new();
/// let argv = kevy_resp::Argv::from(vec![b"INCR".to_vec(), b"n".to_vec()]);
/// let mut out = Vec::new();
/// assert!(kevy_verbs::exec(&mut store, b"INCR", &argv, &mut out).is_some());
/// assert_eq!(out, b":1\r\n");
/// assert!(kevy_verbs::exec(&mut store, b"PING", &argv, &mut out).is_none());
/// ```
pub fn exec<A: ArgvView + ?Sized>(
    store: &mut Store,
    verb: &[u8],
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    if let Some(e) = strings::exec(verb, store, args, out) {
        return Some(e);
    }
    if let Some(e) = bitmap::exec(verb, store, args, out) {
        return Some(e);
    }
    if let Some(e) = hash::exec(verb, store, args, out) {
        return Some(e);
    }
    if let Some(e) = list::exec(verb, store, args, out) {
        return Some(e);
    }
    if let Some(e) = set::exec(verb, store, args, out) {
        return Some(e);
    }
    if let Some(e) = zset::exec(verb, store, args, out) {
        return Some(e);
    }
    if let Some(e) = keyspace::exec(verb, store, args, out) {
        return Some(e);
    }
    #[cfg(feature = "streams-geo")]
    if let Some(e) = geo::exec(verb, store, args, out) {
        return Some(e);
    }
    #[cfg(feature = "streams-geo")]
    if let Some(e) = stream::exec(verb, store, args, out) {
        return Some(e);
    }
    None
}

/// `Write` when `changed`, `Unchanged` otherwise.
fn changed(changed: bool) -> Effect {
    if changed { Effect::Write } else { Effect::Unchanged }
}

#[cfg(test)]
mod tests;

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Effect>();
    send_sync::<Verb>();
    send_sync::<args::ScanOpts>();
    send_sync::<args::ScanOptsError>();
    send_sync::<aof::Claim>();
    send_sync::<aof::Consumer>();
    send_sync::<reply::Scores>();
    #[cfg(feature = "streams-geo")]
    send_sync::<geo::StoreSearchError>();
};
