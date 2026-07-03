//! OP_TABLE ↔ server cross-checks (v2.1 parity CI).
//!
//! The server's semantic classification lists used to be five
//! hand-maintained `match` tables with no cross-checks; these tests
//! ground every one of them against `kevy_resp::ops_table::OP_TABLE`
//! by CALLING the real functions per table row. A new command added
//! to dispatch without a table row (or vice versa) fails here with
//! the exact (op, property) named.

use kevy_resp::ops_table::{KNOWN_GAPS, NotifyKind, OP_TABLE, surface};
use kevy_rt::NotifyClass;

use crate::cmd::{is_growing_write_verb, is_write_verb, notify_class_for_verb};
use crate::cmd_block::wake_idx_for_verb;

#[test]
fn is_write_verb_matches_table() {
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue; // classification fns only know server verbs
        }
        assert_eq!(
            is_write_verb(o.name.as_bytes()),
            o.write,
            "{}: is_write_verb disagrees with OP_TABLE.write",
            o.name
        );
    }
}

#[test]
fn is_growing_write_verb_matches_table() {
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue;
        }
        assert_eq!(
            is_growing_write_verb(o.name.as_bytes()),
            o.growing,
            "{}: is_growing_write_verb disagrees with OP_TABLE.growing",
            o.name
        );
    }
}

#[test]
fn notify_class_matches_table() {
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue;
        }
        let got = notify_class_for_verb(o.name.as_bytes());
        let want = o.notify;
        let matches = match (got, want) {
            (None, None) => true,
            (Some(NotifyClass::String), Some(NotifyKind::String)) => true,
            (Some(NotifyClass::Hash), Some(NotifyKind::Hash)) => true,
            (Some(NotifyClass::List), Some(NotifyKind::List)) => true,
            (Some(NotifyClass::Set), Some(NotifyKind::Set)) => true,
            (Some(NotifyClass::Zset), Some(NotifyKind::Zset)) => true,
            (Some(NotifyClass::Stream), Some(NotifyKind::Stream)) => true,
            (Some(NotifyClass::Generic), Some(NotifyKind::Generic)) => true,
            _ => false,
        };
        assert!(
            matches,
            "{}: notify_class_for_verb = {:?}, OP_TABLE.notify = {:?}",
            o.name, got, want
        );
    }
}

#[test]
fn wake_set_matches_table() {
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue;
        }
        assert_eq!(
            wake_idx_for_verb(o.name.as_bytes()),
            o.wake_idx,
            "{}: wake_idx_for_verb disagrees with OP_TABLE.wake_idx",
            o.name
        );
    }
}

/// Every table row flagged SERVER must have a dispatch site somewhere
/// in the server source (string-literal presence check — coarse but
/// catches "table says SERVER, nothing implements it"). KNOWN_GAPS'
/// F3 rows are the documented holes going the other way.
#[test]
fn server_surface_has_dispatch_literals() {
    let sources = [
        include_str!("dispatch.rs"),
        include_str!("dispatch_collections.rs"),
        include_str!("dispatch_collections_v127.rs"),
        include_str!("dispatch_resp3.rs"),
        include_str!("dispatch_geo/mod.rs"),
        include_str!("dispatch_stream/mod.rs"),
        include_str!("cmd_resolve.rs"),
        include_str!("commands.rs"),
        include_str!("cmd_block.rs"),
        include_str!("ops/mod.rs"),
        include_str!("cmd_lua.rs"),
    ]
    .concat();
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue;
        }
        let lit = format!("b\"{}\"", o.name);
        assert!(
            sources.contains(&lit),
            "{}: OP_TABLE flags SERVER but no dispatch file contains {lit}",
            o.name
        );
    }
    // And the inverse guard for the ledger: an F3 gap op must NOT
    // appear as a dispatch literal (if it does, the gap was closed —
    // update the table + remove the ledger row).
    for (name, flag, _) in KNOWN_GAPS {
        if flag & surface::SERVER == 0 {
            continue;
        }
        let lit = format!("b\"{name}\"");
        assert!(
            !sources.contains(&lit),
            "{name}: ledgered as a SERVER gap but a dispatch literal exists — close the ledger entry"
        );
    }
}
