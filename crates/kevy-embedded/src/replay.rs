//! Apply one logged write to a keyspace: AOF replay on open, a frame a
//! replica receives from its primary, and a frame a host feeds back.
//!
//! A frame is run by the same command layer the server replays its own
//! log with (`kevy_verbs::exec`), so any write either side records, this
//! side can apply. The reply is dropped: it is the value the command
//! returned, not a report on the replay. A frame that fails here was
//! written against a state this keyspace is not in, and there is no
//! channel to say so; Redis aborts in that case, this continues.

use std::cell::RefCell;

use kevy_persist::Argv;
use kevy_store::Store;

thread_local! {
    // one reply buffer per thread, so a replay of millions of frames
    // does not allocate one per frame
    static REPLY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Apply one logged frame to `store`. Reads and unknown verbs are
/// skipped; an unknown verb is how a log written by a newer kevy reads.
pub(crate) fn apply(store: &mut Store, args: &Argv) {
    let Some(name) = args.first() else { return };
    let mut buf = [0u8; 32];
    let verb = kevy_verbs::args::upper_verb(name, &mut buf);
    // the stream and geo writes are the server's alone for now
    if !kevy_verbs::verb(verb).is_some_and(|v| v.write) || kevy_verbs::is_streams_geo(verb) {
        return;
    }
    REPLY.with(|r| {
        let mut out = r.borrow_mut();
        out.clear();
        let _ = kevy_verbs::exec(store, verb, args, &mut out);
    });
}

/// Every verb [`apply`] applies: the shared layer's writes.
#[cfg(test)]
pub(crate) fn replay_verbs() -> Vec<&'static str> {
    kevy_verbs::VERBS
        .iter()
        .filter(|v| v.write && !kevy_verbs::is_streams_geo(v.name.as_bytes()))
        .map(|v| v.name)
        .collect()
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
