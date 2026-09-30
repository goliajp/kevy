//! A catalog change is recorded where it commits, and nowhere else: a read
//! whose reduce overlaps one, on a primary or on a replica, records nothing.

use std::sync::PoisonError;
use std::time::Duration;

use kevy_index::{Catalog, IndexKind, IndexSpec, TableCatalog, TableSpec, ValType};
use kevy_resp::{Argv, RespVersion};
use kevy_rt::Commands;
use kevy_store::Store;

use crate::KevyCommands;

fn argv(cmd: &str) -> Vec<Vec<u8>> {
    cmd.split(' ').map(|w| w.as_bytes().to_vec()).collect()
}

/// Run `during` while a read is inside its reduce on another thread: an
/// `IDX.COUNT`, held where it counts a hit against the usage table until
/// `during` has returned. `during` installs no index catalog, which
/// re-keys that table.
fn overlapped(cmds: &KevyCommands, during: impl FnOnce()) {
    let usage = cmds.state().catalogs.usage.write().unwrap_or_else(PoisonError::into_inner);
    let reader = cmds.clone();
    let read = std::thread::spawn(move || {
        let mut served = vec![crate::cmd_index_query::ST_OK];
        served.extend_from_slice(&0u64.to_le_bytes());
        let query = argv("IDX.COUNT age RANGE 0 10");
        drop(reader.extension_reduce(&query, vec![served.clone(), served], RespVersion::V2));
    });
    // time for the read to reach the usage table and wait on it
    std::thread::sleep(Duration::from_millis(200));
    assert!(!read.is_finished());
    during();
    drop(usage);
    read.join().unwrap();
}

/// The `(lineage, version)` the node's catalog is recorded at.
fn recorded_at(cmds: &KevyCommands) -> (Vec<u8>, Vec<u8>) {
    let frame = crate::catalog_record::snapshot_aux(cmds.state()).unwrap();
    (frame[1].to_vec(), frame[2].to_vec())
}

fn ddl(cmds: &KevyCommands, cmd: &str) {
    let (ctx, mut out) = (cmds.ctx(), Vec::new());
    let args = Argv::from(argv(cmd));
    let verb = cmd.split(' ').next().unwrap().as_bytes();
    assert!(crate::catalog_record::dispatch(&ctx, verb, &mut Store::new(), &args, &mut out));
    assert_eq!(out, b"+OK\r\n", "{cmd}");
}

#[test]
fn a_read_that_overlaps_a_change_on_a_primary_records_nothing() {
    let cmds = KevyCommands::sharded(2);
    ddl(&cmds, "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range");
    let (lineage, version) = recorded_at(&cmds);
    assert_eq!(version, b"1");
    overlapped(&cmds, || ddl(&cmds, "VIEW.CREATE adults QUERY age RANGE 18 200 ORDER BY age"));
    // the view's own frame, and none for the read
    assert_eq!(recorded_at(&cmds), (lineage, b"2".to_vec()));
}

/// The primary's frame at `version` of lineage 7, declaring the indexes
/// `names` and, when `table`, a table `t`.
fn primarys_frame(version: u64, names: &[&[u8]], table: bool) -> Argv {
    let mut cat = Catalog::new();
    for name in names {
        let spec =
            IndexSpec::builder(name.to_vec(), b"u:".to_vec(), IndexKind::Range, ValType::I64)
                .with_field(b"age".to_vec())
                .build()
                .unwrap();
        cat.create(spec).unwrap();
    }
    let mut tables = TableCatalog::new();
    if table {
        let mut t = TableSpec::default();
        (t.name, t.prefix, t.pk) = (b"t".to_vec(), b"t:".to_vec(), b"id".to_vec());
        t.columns = vec![(b"id".to_vec(), ValType::Str)];
        tables.create(t).unwrap();
    }
    let verb = kevy_resp::ops_table::CATALOG.as_bytes().to_vec();
    Argv::from(vec![
        verb,
        b"7".to_vec(),
        version.to_string().into_bytes(),
        cat.to_sidecar().into_bytes(),
        Vec::new(),
        tables.to_sidecar().into_bytes(),
    ])
}

#[test]
fn a_read_that_overlaps_a_primarys_frame_on_a_replica_leaves_the_next_frame_to_apply() {
    let cmds = KevyCommands::sharded(2);
    let state = cmds.state();
    state.replication.force_replica_flag();
    let mut out = Vec::new();
    let mut apply = |frame: Argv| crate::catalog_record::apply(state, &frame, &mut out);
    apply(primarys_frame(1, &[b"a"], false));
    // a frame that changes only the table catalog, so the read stays held
    overlapped(&cmds, || apply(primarys_frame(2, &[b"a"], true)));
    apply(primarys_frame(3, &[b"a", b"b"], true));
    let index = state.catalogs.index().unwrap();
    let held: Vec<&[u8]> = index.iter().map(|(s, _)| s.name()).collect();
    assert_eq!(held, [&b"a"[..], b"b"]);
    assert_eq!(recorded_at(&cmds), (b"7".to_vec(), b"3".to_vec()));
}
