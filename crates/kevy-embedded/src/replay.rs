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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn apply(store: &mut Store, args: &Argv) {
    apply_view(store, args);
}

/// Apply one logged frame to `store`, as the argv shape the dispatcher
/// runs the command layer with.
///
/// The command layer is generic over the argv type, so a replay that
/// hands it a `kevy_resp::Argv` beside a dispatcher that hands it
/// `&[Vec<u8>]` links every command twice: about 50 KB of a browser
/// module that is shipped over the wire. There the frame is copied into
/// a buffer whose vectors are kept between frames, and one instance
/// serves both paths. Native builds keep the zero-copy call.
#[cfg(target_arch = "wasm32")]
pub(crate) fn apply(store: &mut Store, args: &Argv) {
    thread_local! {
        static OWNED: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    }
    OWNED.with(|o| {
        let mut owned = o.borrow_mut();
        owned.truncate(args.len());
        for (i, arg) in args.iter().enumerate() {
            match owned.get_mut(i) {
                Some(slot) => {
                    slot.clear();
                    slot.extend_from_slice(arg);
                }
                None => owned.push(arg.to_vec()),
            }
        }
        apply_view(store, &crate::dispatch::Args(&owned));
    });
}

fn apply_view<A: kevy_resp::ArgvView + ?Sized>(store: &mut Store, args: &A) {
    let Some(name) = args.get(0) else { return };
    let mut buf = [0u8; 32];
    let verb = kevy_verbs::args::upper_verb(name, &mut buf);
    if !serves_family(verb) {
        return;
    }
    REPLY.with(|r| {
        let mut out = r.borrow_mut();
        out.clear();
        // an internal record frame is applied here, never from a client.
        // The only one is a stream record, which `serves_family` refused
        // above in a build without streams; the cfg keeps the stream code
        // it reaches out of that build as well.
        #[cfg(feature = "streams-geo")]
        if kevy_verbs::aof::apply_internal(store, args, &mut out) {
            return;
        }
        if kevy_verbs::verb(verb).is_some_and(|v| v.write) {
            let _ = kevy_verbs::exec(store, verb, args, &mut out);
        }
    });
}

/// Whether this build applies the verb's family: the stream and geo
/// writes only with the `streams-geo` feature, whatever another crate in
/// the same build turned on in the shared layer.
fn serves_family(verb: &[u8]) -> bool {
    cfg!(feature = "streams-geo") || !kevy_verbs::is_streams_geo(verb)
}

/// Every verb [`apply`] applies: the shared layer's writes and the
/// internal record verbs.
#[cfg(test)]
pub(crate) fn replay_verbs() -> Vec<&'static str> {
    let internal = kevy_resp::ops_table::CONSUMER_SEEN;
    kevy_verbs::VERBS
        .iter()
        .filter(|v| v.write)
        .map(|v| v.name)
        .chain([internal])
        .filter(|name| serves_family(name.as_bytes()))
        .collect()
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
