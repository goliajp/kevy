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
mod hash;
mod hash_ttl;
mod keyspace;
mod list;
mod list_move;
pub mod reply;
mod set;
mod strings;
mod verbs;
mod zset;
mod zset_range;

pub use verbs::{VERBS, Verb, verb};

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
    /// Record nothing, not even the argv: a random command that removed
    /// nothing.
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
    keyspace::exec(verb, store, args, out)
}

/// `Write` when `changed`, `Unchanged` otherwise.
fn changed(changed: bool) -> Effect {
    if changed { Effect::Write } else { Effect::Unchanged }
}

#[cfg(test)]
mod tests;
