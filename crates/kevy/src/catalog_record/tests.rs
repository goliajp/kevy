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
    let held = snapshot_aux(state);
    assert_ne!(held[1], b"0"[..], "the create was recorded");
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
    assert_eq!(snapshot_aux(state), held);
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
    assert_eq!(*state.catalogs.record.lock(), (0, 0), "nothing was adopted");
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
    assert_eq!(*cmds.state().catalogs.record.lock(), (0, 0), "no version was minted");
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
    assert_eq!(*state.catalogs.record.lock(), (0, 0));
}

/// A node whose every shard has finished its startup restore.
fn restored(nshards: usize) -> KevyCommands {
    let cmds = KevyCommands::sharded(nshards);
    for _ in 0..nshards {
        shard_restored(cmds.state(), &mut |_| true);
    }
    cmds
}

/// Shard Y's full sync is served from a snapshot taken before the
/// primary's first change; shard X's stream then carries that change and
/// is applied first. The older snapshot landing last keeps the catalog.
#[test]
fn a_full_sync_older_than_frames_another_shard_applied_keeps_the_catalog() {
    use kevy_rt::Commands;
    let primary = restored(2);
    let older = primary.snapshot_aux();
    let index = "IDX.CREATE age ON PREFIX u: FIELD age TYPE i64 KIND range";
    primary.dispatch(&mut Store::new(), &Argv::from(words(index)));
    let frame = primary.snapshot_aux().expect("the create was recorded");
    let replica = restored(2);
    replica.state().replication.force_replica_flag();
    apply(replica.state(), &frame, &mut Vec::new());
    replica.load_snapshot_aux(older.as_ref(), true);
    assert!(replica.state().catalogs.index().is_some_and(|c| c.get(b"age").is_some()));
    let held = replica.snapshot_aux().unwrap();
    assert_eq!((&held[1], &held[2]), (&frame[1], &frame[2]));
}

/// A 6.4 primary's snapshot carries no catalog frame, because it
/// replicates no catalog: a full sync from it drops the catalog the
/// replica held, while a restore of such a snapshot keeps it.
#[test]
fn a_full_sync_from_a_snapshot_with_no_catalog_frame_drops_the_catalog() {
    use kevy_rt::Commands;
    let primary = restored(1);
    let index = "IDX.CREATE age ON PREFIX u: FIELD age TYPE i64 KIND range";
    primary.dispatch(&mut Store::new(), &Argv::from(words(index)));
    let frame = primary.snapshot_aux().expect("the create was recorded");
    let replica = restored(1);
    replica.state().replication.force_replica_flag();
    apply(replica.state(), &frame, &mut Vec::new());
    let holds_age =
        |c: &KevyCommands| c.state().catalogs.index().is_some_and(|i| i.get(b"age").is_some());
    replica.load_snapshot_aux(None, false);
    assert!(holds_age(&replica), "a restore without a frame changes nothing");
    replica.load_snapshot_aux(None, true);
    assert!(!holds_age(&replica));
    assert_eq!(*replica.state().catalogs.record.lock(), (0, 0));
    replica.load_snapshot_aux(None, true);
    assert_eq!(*replica.state().catalogs.record.lock(), (0, 0), "nothing held, nothing to drop");
}
