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
use crate::replay::replay_verbs;

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
    diff("REPLAY", &replay_verbs(), surface::REPLAY);
}

/// Grounding: every verb the replay claims is one it really applies —
/// a frame of it changes an empty keyspace or is refused, never skipped.
#[test]
fn replay_manifest_verbs_are_applied() {
    let verbs = replay_verbs();
    assert!(verbs.len() > 50, "the replay claims only {} verbs", verbs.len());
    for v in verbs {
        let mut buf = [0u8; 32];
        let up = kevy_verbs::args::upper_verb(v.as_bytes(), &mut buf);
        let argv = kevy_persist::Argv::from(vec![v.as_bytes().to_vec()]);
        let mut out = Vec::new();
        let ran = kevy_verbs::exec(&mut kevy_store::Store::new(), up, &argv, &mut out);
        assert!(ran.is_some() && !out.is_empty(), "the replay lists {v} but nothing runs it");
    }
}
