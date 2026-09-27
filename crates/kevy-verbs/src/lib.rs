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
use kevy_store::Store;

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
pub mod reply;
mod set;
#[cfg(feature = "streams-geo")]
mod stream;
mod strings;
mod verbs;
mod zset;
mod zset_range;

pub use verbs::{VERBS, Verb, is_streams_geo, verb};

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
    Read,
    /// A write: record the argv as it was run.
    Write,
    /// A write that changed nothing this time, such as `SET … NX` on a
    /// key that exists or `HDEL` of a missing field. Recording the argv
    /// is harmless; a caller that records only changes can skip it.
    Unchanged,
    /// Record this frame instead of the argv. A command whose effect is
    /// random (`SPOP`) is recorded as what it did (`SREM key member…`),
    /// so replaying the record cannot pick differently.
    Record(Vec<Vec<u8>>),
    /// Record these frames, in order, instead of the argv: a command whose
    /// effect depends on the clock, recorded as what it did. A claim gated
    /// on idle time is one `XCLAIM … TIME t RETRYCOUNT n FORCE JUSTID` per
    /// outcome, so a replay at another time gives the same owners and
    /// counts.
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
    /// let Some(Effect::RecordAll(frames)) = exec(&mut store, b"XCLAIM", &claim, &mut Vec::new()) else {
    ///     panic!("a claim is recorded as its outcome")
    /// };
    /// assert_eq!(&frames[0][..6], [&b"XCLAIM"[..], b"s", b"g", b"b", b"0", b"1-1"]);
    /// ```
    RecordAll(Vec<Vec<Vec<u8>>>),
    /// Record nothing, not even the argv: a random command that removed
    /// nothing, or a claim that changed nothing.
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
