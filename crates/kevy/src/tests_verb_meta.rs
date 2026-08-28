//! VERB_META ↔ dispatch parity (CI face).
//!
//! The doc table and the real command surface must stay bidirectionally
//! equal: a verb reachable in dispatch without a doc row (or a doc row
//! for nothing) fails here by name. The OP_TABLE server subset is also
//! held ⊆ VERB_META so the two registries can't drift apart.

use std::collections::HashSet;

use kevy_resp::ops_table::{OP_TABLE, surface};

use crate::verb_meta::{VERB_META, verb_meta};

/// Verbs dispatch reaches that are deliberately NOT documented:
/// internal fan-out continuations (not wire-facing top-level verbs).
const UNDOCUMENTED_INTERNAL: &[&str] = &["AGG.FETCH", "VIEW.HYDRATE"];

#[test]
fn op_table_server_verbs_all_documented() {
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue;
        }
        assert!(
            verb_meta(o.name).is_some(),
            "{}: in OP_TABLE (SERVER) but missing from VERB_META",
            o.name
        );
    }
}

#[test]
fn no_duplicate_meta_rows() {
    let mut seen = HashSet::new();
    for m in VERB_META {
        assert!(seen.insert(m.name), "{}: duplicate VERB_META row", m.name);
    }
}

#[test]
fn meta_rows_are_well_formed() {
    for m in VERB_META {
        assert!(!m.summary.is_empty(), "{}: empty summary", m.name);
        assert!(
            m.syntax.starts_with(m.name),
            "{}: syntax must start with the verb itself (got '{}')",
            m.name,
            m.syntax
        );
        assert!(m.arity != 0, "{}: arity 0 is meaningless", m.name);
        assert!(
            m.flags.contains(&"write") ^ m.flags.contains(&"readonly"),
            "{}: exactly one of write/readonly required",
            m.name
        );
    }
}

#[test]
fn command_docs_covers_every_meta_row_and_nothing_else() {
    // Drive the REAL encoder: COMMAND LIST must round-trip the table.
    let mut out = Vec::new();
    let mut args = kevy_resp::Argv::with_capacity(2, 16);
    args.push(b"COMMAND");
    args.push(b"LIST");
    crate::cmd_command::cmd_command(&args, &mut out);
    let text = String::from_utf8_lossy(&out);
    for m in VERB_META {
        assert!(
            text.contains(&format!("\r\n{}\r\n", m.name)),
            "{}: missing from COMMAND LIST wire reply",
            m.name
        );
    }
}

#[test]
fn internal_continuations_stay_undocumented() {
    for v in UNDOCUMENTED_INTERNAL {
        assert!(
            verb_meta(v).is_none(),
            "{v}: internal continuation must not be documented as wire-facing"
        );
    }
}

/// The standalone `route()` face and the hot-path `resolve().route`
/// must answer identically for every registered verb. Swept across
/// argc 1..=8 because most routing arms key off `args.len()` guards.
#[test]
fn route_matches_resolve_route_for_every_verb() {
    use kevy_rt::Commands;
    let c = crate::KevyCommands::new();
    for m in VERB_META {
        for argc in 1..=8usize {
            let mut parts: Vec<Vec<u8>> = vec![m.name.as_bytes().to_vec()];
            parts.resize(argc, b"1".to_vec());
            let a = kevy_rt::Argv::from(parts);
            assert_eq!(
                c.route(&a),
                c.resolve(&a).route,
                "{} argc={argc}: route() and resolve().route disagree",
                m.name
            );
        }
    }
}


/// The DOC face and the AOF/propagation gate answer different questions,
/// and for these verbs they answer differently. `OP_TABLE.write` asks
/// "does this verb itself produce the replayable data effect?";
/// `VERB_META.flags` asks what `COMMAND DOCS` should tell a client — and
/// a client uses that to decide what never to send to a read-only
/// replica. Marking these `readonly` in the doc face would be a lie to
/// every client that routes on it.
///
/// The ledger is EXACT, in the manner of `ops_table::KNOWN_GAPS`: a new
/// divergence fails here by name, and so does removing one that still
/// exists. Reasons are the ones OP_TABLE states at each row.
const WRITE_FLAG_DIVERGES: &[(&str, &str)] = &[
    ("BLPOP", "the blocked-serve path executes and AOF-logs the effect as a plain LPOP"),
    ("BRPOP", "the blocked-serve path executes and AOF-logs the effect as a plain RPOP"),
    ("RENAME", "routed at the runtime Op level (Route::Rename -> exec_op synthesis)"),
    ("RENAMENX", "routed at the runtime Op level (Route::Rename -> exec_op synthesis)"),
    ("IDX.CREATE", "catalog mutation, sidecar-persisted; indexes are derived state"),
    ("IDX.DROP", "catalog mutation, sidecar-persisted; indexes are derived state"),
    ("IDX.REBUILD", "catalog mutation, sidecar-persisted; indexes are derived state"),
    ("VIEW.CREATE", "catalog mutation, sidecar-persisted; views are derived state"),
    ("VIEW.DROP", "catalog mutation, sidecar-persisted; views are derived state"),
    ("VIEW.REBUILD", "catalog mutation, sidecar-persisted; views are derived state"),
    ("TABLE.DECLARE", "catalog op, sidecar-persisted - same reasoning as IDX.*"),
    ("TABLE.ENSURE", "catalog op, sidecar-persisted - same reasoning as IDX.*"),
    ("TABLE.REPLACE", "catalog op, sidecar-persisted - same reasoning as IDX.*"),
    ("TABLE.DROP", "catalog op, sidecar-persisted - same reasoning as IDX.*"),
];

/// Every SERVER verb's `write` flag agrees between the two registries,
/// except the ones registered above — checked in both directions, so the
/// ledger can neither grow silently nor keep an entry that has healed.
///
/// Nothing held this before: the sibling test above asks whether every
/// OP_TABLE verb HAS a doc row, never whether the two rows AGREE. The
/// module header meanwhile claimed the doc face mirrors OP_TABLE's write
/// column literally, which all fourteen of these have never done.
#[test]
fn the_write_flag_diverges_only_where_registered() {
    let registered: HashSet<&str> = WRITE_FLAG_DIVERGES.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        registered.len(),
        WRITE_FLAG_DIVERGES.len(),
        "the divergence ledger names a verb twice"
    );

    let mut observed = HashSet::new();
    let mut compared = 0usize;
    for o in OP_TABLE {
        if o.surfaces & surface::SERVER == 0 {
            continue;
        }
        let Some(m) = verb_meta(o.name) else { continue };
        compared += 1;
        if m.flags.contains(&"write") != o.write {
            observed.insert(o.name);
        }
    }
    // A floor: an empty or near-empty comparison would pass every
    // assertion below while proving nothing about either table.
    assert!(
        compared >= 100,
        "only {compared} verbs carried both a doc row and an OP_TABLE row — \
         one of the two registries did not load"
    );

    let unregistered: Vec<&str> = observed.difference(&registered).copied().collect();
    assert!(
        unregistered.is_empty(),
        "{unregistered:?} disagree about `write` between VERB_META and OP_TABLE \
         with no entry in WRITE_FLAG_DIVERGES. Either the doc face is wrong, or \
         the divergence is deliberate and belongs in the ledger with its reason."
    );

    let healed: Vec<&str> = registered.difference(&observed).copied().collect();
    assert!(
        healed.is_empty(),
        "{healed:?} are registered as diverging but now agree — drop them from \
         WRITE_FLAG_DIVERGES so the ledger stays exact"
    );
}

/// `kevy_resp::verb_arity::VERB_ARITY` is the same column, in the crate a
/// surface outside `kevy` can read. Held in exact bijection here: same
/// names, same numbers, both directions. A verb added to one table and
/// not the other fails by name, and so does an arity edited on one side.
///
/// Without this the copy is what the facade's `argv.len() < 4` literals
/// were — a number beside a comment naming its source, which is a drift
/// one step before it happens.
#[test]
fn verb_arity_is_the_same_column_as_verb_meta() {
    use kevy_resp::verb_arity::VERB_ARITY;

    assert!(
        VERB_META.len() > 150 && VERB_ARITY.len() > 150,
        "VERB_META {} rows / VERB_ARITY {} rows — a table did not load, and an \
         empty comparison passes every assertion below",
        VERB_META.len(),
        VERB_ARITY.len()
    );

    let meta: HashSet<&str> = VERB_META.iter().map(|m| m.name).collect();
    let shared: HashSet<&str> = VERB_ARITY.iter().map(|(n, _)| *n).collect();

    let missing: Vec<&str> = meta.difference(&shared).copied().collect();
    assert!(
        missing.is_empty(),
        "{missing:?} are documented in VERB_META with no row in \
         kevy_resp::verb_arity::VERB_ARITY — add them there, or a surface \
         outside kevy has to restate the number"
    );

    let extra: Vec<&str> = shared.difference(&meta).copied().collect();
    assert!(
        extra.is_empty(),
        "{extra:?} carry an arity in kevy-resp with no VERB_META row — the \
         shared table may not describe a verb this engine does not document"
    );

    let mismatched: Vec<String> = VERB_META
        .iter()
        .filter_map(|m| {
            let shared = kevy_resp::verb_arity::arity_of(m.name)?;
            (shared != m.arity).then(|| format!("{}: VERB_META {} / shared {}", m.name, m.arity, shared))
        })
        .collect();
    assert!(mismatched.is_empty(), "the two arity columns disagree: {mismatched:?}");
}
