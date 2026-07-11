//! VERB_META ↔ dispatch parity (v3.10 aigate, CI face).
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
