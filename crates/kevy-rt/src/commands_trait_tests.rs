//! The optional `on_*` hooks have default bodies so an existing
//! [`Commands`](crate::Commands) implementor gains them without
//! changing — and a default body nothing calls is a never-executed
//! region. The first one showed up dead in coverage the day it was added.
//!
//! Split out of `commands_trait.rs`, which crossed the 500-LOC house
//! rule the moment a second hook arrived with its example.

use crate::{ArgvView, Commands, Route, Store, TxnKind};

/// The smallest thing that can be a `Commands`: the five required
/// methods and nothing else.
#[derive(Clone)]
pub(crate) struct Minimal;

impl Commands for Minimal {
    fn route<A: ArgvView + ?Sized>(&self, _a: &A) -> Route {
        Route::Local
    }
    fn dispatch<A: ArgvView + ?Sized>(&self, _s: &mut Store, _a: &A) -> Vec<u8> {
        b"+OK\r\n".to_vec()
    }
    fn is_quit<A: ArgvView + ?Sized>(&self, _a: &A) -> bool {
        false
    }
    fn is_write<A: ArgvView + ?Sized>(&self, _a: &A) -> bool {
        false
    }
    fn txn_kind<A: ArgvView + ?Sized>(&self, _a: &A) -> TxnKind {
        TxnKind::Other
    }
}

/// The optional hooks have default bodies so an existing
/// implementor gains them without changing — and a default body
/// nothing calls is a never-executed region, which is how this test
/// came to exist: `on_query_buffer_exceeded` showed up dead in coverage the
/// day it was added. Calling them from the smallest possible
/// implementor is both the coverage and the claim: this trait can
/// be implemented with five methods.
#[test]
fn the_optional_hooks_default_to_doing_nothing() {
    let c = Minimal;
    c.on_query_buffer_exceeded();
    c.on_data_dir(std::path::Path::new("/tmp/nowhere"));
    c.on_tick_gap(0);
    c.on_persist_stats(false, 0);
    c.on_aof_format(0);
    c.on_conn_gauge(0);
    c.on_blocked_gauge(0);
}

/// The extension and snapshot-aux hooks default to "nothing here": no
/// targets, no aux frame to save, and applying or loading one changes
/// nothing in the store.
#[test]
fn the_extension_and_aux_hooks_default_to_nothing() {
    let c = Minimal;
    let mut store = Store::new();
    let argv = vec![b"EXT.OP".to_vec(), b"k".to_vec()];
    assert_eq!(c.extension_targets(&argv), None);
    c.apply_ext(&mut store, b"payload");
    assert_eq!(c.snapshot_aux(), None);
    let frame = kevy_resp::Argv::from(argv);
    c.load_snapshot_aux(Some(&frame), true);
    c.load_snapshot_aux(None, false);
    assert_eq!(store.dbsize(), 0, "a default hook wrote to the store");
}

/// Without an override, a verb-tagged dispatch answers on either
/// protocol exactly as `dispatch` does, whatever verb it carries.
#[test]
fn a_verb_tagged_dispatch_defaults_to_dispatch_on_both_protocols() {
    use kevy_resp::RespVersion;
    let c = Minimal;
    let mut store = Store::new();
    let argv = kevy_resp::Argv::from(vec![b"PING".to_vec()]);
    let verb = crate::VerbId::new(std::hint::black_box(7));
    assert_eq!(verb.get(), 7);
    assert_ne!(verb, crate::VerbId::UNKNOWN);
    for proto in [RespVersion::V2, RespVersion::V3] {
        let mut out = Vec::new();
        c.dispatch_verb_into(&mut store, &argv, verb, proto, &mut out);
        assert_eq!(out, b"+OK\r\n", "{proto:?}");
    }
}
