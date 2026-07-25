//! OP_TABLE ↔ kevy-embedded surface parity CI.
//!
//! Each embedded surface exports a manifest of implemented command
//! names; these tests assert manifest == the table's surface-flag set,
//! failing with the exact missing/extra names. This is the structural
//! fix for the manifest-drift class (facade verbs the replay couldn't
//! parse = silent data loss).

use std::collections::BTreeSet;

use kevy_resp::ops_table::{ops_with, surface};

use crate::op_manifest::ESTORE_OPS;
use crate::ops_atomic::ATOMIC_OPS;
use crate::ops_atomic_all::ATOMIC_ALL_OPS;
use crate::ops_pipeline::PIPELINE_OPS;
use crate::replay::REPLAY_VERBS;

fn diff(surface_name: &str, manifest: &[&str], flag: u16) {
    let m: BTreeSet<&str> = manifest.iter().copied().collect();
    let t: BTreeSet<&str> = ops_with(flag).into_iter().collect();
    let missing: Vec<_> = t.difference(&m).collect();
    let extra: Vec<_> = m.difference(&t).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{surface_name} manifest != OP_TABLE: table-but-not-manifest {missing:?}, \
         manifest-but-not-table {extra:?}"
    );
}

#[test]
fn pipeline_manifest_matches_table() {
    diff("PIPE", PIPELINE_OPS, surface::PIPE);
}

#[test]
fn atomic_manifests_match_table_and_each_other() {
    // The two atomic ctxs must NEVER drift (zscore went missing from
    // AtomicAllShards once already).
    let a: BTreeSet<&str> = ATOMIC_OPS.iter().copied().collect();
    let b: BTreeSet<&str> = ATOMIC_ALL_OPS.iter().copied().collect();
    assert_eq!(a, b, "AtomicCtx and AtomicAllShards manifests drifted");
    diff("ATOMIC", ATOMIC_OPS, surface::ATOMIC);
}

#[test]
fn estore_manifest_matches_table() {
    diff("ESTORE", ESTORE_OPS, surface::ESTORE);
}

#[test]
fn replay_manifest_matches_table() {
    diff("REPLAY", REPLAY_VERBS, surface::REPLAY);
}

/// Grounding: every manifest verb the replay claims must actually
/// have a literal arm in replay.rs source.
#[test]
fn replay_manifest_verbs_have_source_arms() {
    let src = include_str!("replay.rs");
    for v in REPLAY_VERBS {
        let lit = format!("b\"{v}\"");
        assert!(
            src.contains(&lit),
            "REPLAY_VERBS lists {v} but replay.rs has no {lit} arm"
        );
    }
}
