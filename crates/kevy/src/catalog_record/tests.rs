//! A catalog frame that does not parse changes nothing, and the paths a
//! node takes when it has nothing to record.

use std::sync::Arc;

use kevy_resp::Argv;

use super::*;
use crate::KevyCommands;
use crate::index_runtime::test_shard::words;

fn frame(parts: &[&[u8]]) -> Argv {
    let mut v = vec![b"XINTERNAL.CATALOG".to_vec()];
    v.extend(parts.iter().map(|p| p.to_vec()));
    Argv::from(v)
}

fn applied(state: &RuntimeState, parts: &[&[u8]]) -> String {
    let mut out = Vec::new();
    apply(state, &frame(parts), &mut out);
    String::from_utf8(out).unwrap()
}

const MALFORMED: &str = "-ERR malformed catalog record\r\n";

#[test]
fn a_frame_missing_or_mangling_any_part_is_refused_and_changes_nothing() {
    let cmds = KevyCommands::new();
    let state = cmds.state();
    let index = "IDX.CREATE age ON PREFIX u: FIELD age TYPE i64 KIND range";
    let mut store = Store::new();
    cmds.dispatch(&mut store, &Argv::from(words(index)));
    let held = snapshot_aux(state).expect("the create was recorded");
    let good_index = held[3].to_vec();
    let cases: [&[&[u8]]; 11] = [
        &[],
        &[b"\xff"],
        &[b"one"],
        &[b"9999999999999", b"two"],
        &[b"9999999999999", b"1"],
        &[b"9999999999999", b"1", b""],
        &[b"9999999999999", b"1", b"", b""],
        &[b"9999999999999", b"1", b"\xff", b"", b""],
        &[b"9999999999999", b"1", b"not a catalog", b"", b""],
        &[b"9999999999999", b"1", &good_index, b"not a catalog", b""],
        &[b"9999999999999", b"1", &good_index, b"", b"not a catalog"],
    ];
    for parts in cases {
        assert_eq!(applied(state, parts), MALFORMED, "{parts:?}");
    }
    assert!(state.catalogs.index().is_some_and(|c| c.get(b"age").is_some()));
    assert_eq!(snapshot_aux(state), Some(held));
}

#[test]
fn a_newer_frame_with_every_catalog_empty_drops_every_declaration() {
    let cmds = KevyCommands::new();
    let state = cmds.state();
    let mut store = Store::new();
    let index = "IDX.CREATE age ON PREFIX u: FIELD age TYPE i64 KIND range";
    cmds.dispatch(&mut store, &Argv::from(words(index)));
    let (lineage, version) = {
        let at = state.catalogs.record.lock();
        (at.0.to_string(), (at.1 + 1).to_string())
    };
    assert_eq!(applied(state, &[lineage.as_bytes(), version.as_bytes(), b"", b"", b""]), "+OK\r\n");
    assert!(state.catalogs.index().is_none_or(|c| c.get(b"age").is_none()));
}

#[test]
fn a_snapshot_whose_catalog_frame_does_not_parse_leaves_the_catalog_as_it_was() {
    let cmds = KevyCommands::new();
    let state = cmds.state();
    load_snapshot_aux(state, Some(&frame(&[b"1", b"1", b"not a catalog", b"", b""])), false);
    assert!(state.catalogs.index().is_none_or(|c| c.is_empty()));
    assert!(snapshot_aux(state).is_none(), "nothing was adopted");
}

#[test]
fn a_catalog_change_applied_from_a_record_records_no_frame_of_its_own() {
    let cmds = KevyCommands::new();
    let mut store = Store::new();
    let index = "IDX.CREATE age ON PREFIX u: FIELD age TYPE i64 KIND range";
    {
        let _replaying = kevy_rt::RecordApplyGuard::enter();
        cmds.dispatch(&mut store, &Argv::from(words(index)));
    }
    assert!(cmds.state().catalogs.index().is_some_and(|c| c.get(b"age").is_some()));
    assert!(snapshot_aux(cmds.state()).is_none(), "no version was minted");
    kevy_rt::propagation::discard_override();
}

#[test]
fn only_the_catalog_verbs_are_taken_as_catalog_commands() {
    let cmds = KevyCommands::new();
    let mut store = Store::new();
    let mut out = Vec::new();
    let args = Argv::from(words("PING"));
    assert!(!dispatch(&cmds.ctx(), b"PING", &mut store, &args, &mut out));
    assert!(out.is_empty());
}

#[test]
fn a_sidecar_that_is_not_text_reads_as_unreadable_and_an_absent_one_as_empty() {
    let dir = kevy_tmpdir::TmpDir::new("catalog-sidecar-unreadable");
    std::fs::write(dir.path().join(SIDECARS[0]), b"\xff\xfe").unwrap();
    assert!(read_sidecar(dir.path(), SIDECARS[0], Catalog::from_sidecar).is_none());
    let absent = read_sidecar(dir.path(), SIDECARS[1], ViewCatalog::from_sidecar);
    assert!(absent.is_some_and(|c| c.is_empty()));
}

#[test]
fn the_last_shard_restored_without_a_data_directory_imports_nothing() {
    let mut cfg = kevy_config::Config::default();
    cfg.server.data_dir = std::path::PathBuf::new();
    let state = RuntimeState::new(Arc::new(cfg), std::path::PathBuf::new(), 1).unwrap();
    let mut recorded = 0;
    shard_restored(&state, &mut |_| {
        recorded += 1;
        true
    });
    assert_eq!(recorded, 0);
    assert!(snapshot_aux(&state).is_none());
}
