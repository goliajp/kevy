//! The optional `on_*` hooks have default bodies so an existing
//! [`Commands`](crate::Commands) implementor gains them without
//! changing — and a default body nothing calls is a never-executed
//! region. `deadgate` named the first one the day it was added.
//!
//! Split out of `commands_trait.rs`, which crossed the 500-LOC house
//! rule the moment a second hook arrived with its example.

use crate::{ArgvView, Commands, Route, Store, TxnKind};

/// The smallest thing that can be a `Commands`: the five required
/// methods and nothing else.
#[derive(Clone)]
struct Minimal;

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
/// came to exist: `deadgate` named `on_query_buffer_exceeded` the
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
}
