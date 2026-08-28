//! OP_TABLE ↔ server cross-checks (parity CI).
//!
//! The server's semantic classification lists used to be five
//! hand-maintained `match` tables with no cross-checks; these tests
//! ground every one of them against `kevy_resp::ops_table::OP_TABLE`
//! by CALLING the real functions per table row. A new command added
//! to dispatch without a table row (or vice versa) fails here with
//! the exact (op, property) named.

use kevy_resp::ops_table::{KNOWN_GAPS, NotifyKind, OP_TABLE, surface};

use crate::verb_meta::VERB_META;
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
        let matches = matches!(
            (got, want),
            (None, None)
                | (Some(NotifyClass::String), Some(NotifyKind::String))
                | (Some(NotifyClass::Hash), Some(NotifyKind::Hash))
                | (Some(NotifyClass::List), Some(NotifyKind::List))
                | (Some(NotifyClass::Set), Some(NotifyKind::Set))
                | (Some(NotifyClass::Zset), Some(NotifyKind::Zset))
                | (Some(NotifyClass::Stream), Some(NotifyKind::Stream))
                | (Some(NotifyClass::Generic), Some(NotifyKind::Generic))
        );
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


/// VERB_META families whose verbs carry no keyspace semantics at all,
/// and so have no OP_TABLE row.
///
/// OP_TABLE records write classification, notification class, wake index
/// and surfaces — every one of them a property of a keyspace EFFECT. A
/// connection, transaction, pub/sub, script or replication verb has none
/// of those, and consistently has no row.
const NON_KEYSPACE_GROUPS: &[&str] = &["connection", "tx", "pubsub", "script", "replication"];

/// The `server` family is the one that does not split by group: DBSIZE,
/// FLUSHALL and FLUSHDB read or empty the keyspace and DO have rows,
/// while the rest administer the process. So this family is exempted by
/// NAME rather than wholesale — an exemption that would otherwise have
/// covered FLUSHALL, which is exactly the kind of verb a registry must
/// not lose.
const ADMIN_VERBS: &[&str] = &[
    "BGREWRITEAOF",
    "BGSAVE",
    "CLIENT",
    "CLUSTER",
    "COMMAND",
    "CONFIG",
    "DEBUG",
    "INFO",
    "MEMORY",
    "SAVE",
    "SHUTDOWN",
    "SLOWLOG",
];

/// Every documented verb in a KEYSPACE family has an OP_TABLE row.
///
/// The direction nothing held. `op_table_server_verbs_all_documented`
/// asks whether every registry row has a doc row; this asks the reverse,
/// and the reverse is where two verbs were hiding: `HPTTL`, beside four
/// hash-TTL siblings that all had rows, and `IDX.EXPLAIN`, the third of
/// a server-only trio whose other two were registered. Neither could
/// fail any check that iterates OP_TABLE, because neither was in it.
///
/// Both ledgers are EXACT: a family or a verb that acquires a row fails
/// here rather than sitting on an exemption it no longer needs.
#[test]
fn every_keyspace_verb_has_a_registry_row() {
    use std::collections::BTreeSet;

    let rows: BTreeSet<&str> = OP_TABLE.iter().map(|o| o.name).collect();
    let groups: BTreeSet<&str> = NON_KEYSPACE_GROUPS.iter().copied().collect();
    let admin: BTreeSet<&str> = ADMIN_VERBS.iter().copied().collect();
    assert!(VERB_META.len() > 150, "VERB_META did not load");

    let missing: Vec<&str> = VERB_META
        .iter()
        .filter(|m| !groups.contains(m.group) && !admin.contains(m.name) && !rows.contains(m.name))
        .map(|m| m.name)
        .collect();
    assert!(
        missing.is_empty(),
        "{missing:?} are documented keyspace verbs with no OP_TABLE row — add the row, or \
         name the verb in ADMIN_VERBS with its reason"
    );

    let healed_groups: Vec<&str> = NON_KEYSPACE_GROUPS
        .iter()
        .filter(|g| VERB_META.iter().any(|m| m.group == **g && rows.contains(m.name)))
        .copied()
        .collect();
    assert!(
        healed_groups.is_empty(),
        "{healed_groups:?} are named as carrying no keyspace semantics but have rows now"
    );

    let healed_verbs: Vec<&str> = ADMIN_VERBS.iter().filter(|v| rows.contains(*v)).copied().collect();
    assert!(
        healed_verbs.is_empty(),
        "{healed_verbs:?} are named as administrative but have rows now — drop them from \
         ADMIN_VERBS so the ledger stays exact"
    );
}
