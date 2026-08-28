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

/// Every non-test Rust source under `src/`, concatenated, with the file
/// count beside it. Test files are excluded: a verb named only in test
/// data is not a dispatch site, and counting it would let the check pass
/// on a verb nothing implements.
fn server_sources() -> (String, usize) {
    // Every read here panics rather than skipping. A walk that steps
    // over an unreadable file answers a smaller question than the one
    // it was asked, and reports the answer in the same shape — the
    // check would say "nothing implements APPEND" when what happened
    // is that a file could not be opened.
    fn walk(dir: &std::path::Path, out: &mut String, files: &mut usize) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
        for e in entries {
            let path = e.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, out, files);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name.starts_with("tests") || name.contains("_tests") {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                out.push_str(&text);
                *files += 1;
            }
        }
    }
    let (mut out, mut files) = (String::new(), 0);
    walk(std::path::Path::new("src"), &mut out, &mut files);
    (out, files)
}

/// Every table row flagged SERVER must have a dispatch site somewhere
/// in the server source (string-literal presence check — coarse but
/// catches "table says SERVER, nothing implements it"). KNOWN_GAPS'
/// F3 rows are the documented holes going the other way.
///
/// The source set used to be eleven `include_str!` lines written out by
/// hand. Splitting `dispatch_string` into its own module was enough to
/// blind it: APPEND's arm moved to a file nobody had added to the list,
/// and the check reported that nothing on the server implemented
/// APPEND. A hand-copied file list answers a question about the files
/// someone remembered, so this walks the tree instead.

#[test]
fn server_surface_has_dispatch_literals() {
    let (sources, files) = server_sources();
    // A floor. An empty read would fail the first assertion loudly and
    // pass the inverse guard silently — which is the direction that
    // matters, since the inverse guard is what says a gap is still open.
    assert!(
        files > 20 && sources.len() > 200_000,
        "the source walk found {files} files / {} bytes — it is broken, \
         not the server empty",
        sources.len()
    );
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
