use kevy_rt::{Argv, Commands};
use kevy_store::Store;

use crate::KevyCommands;

/// A shard walks its tables, then loads a snapshot (a replica's full sync
/// after another shard installed the catalog, or a resync under the same
/// catalog): the loaded rows take the packed form as the walked ones did.
#[test]
fn the_rows_a_snapshot_load_puts_in_are_packed() {
    let (cmds, mut store) = (KevyCommands::new(), Store::new());
    store.set_packed_rows(true);
    let declare = "TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str";
    let argv = Argv::from(declare.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
    assert_eq!(cmds.dispatch(&mut store, &argv), b"+OK\r\n");
    cmds.on_shard_tick(&mut store);
    store.flushall();
    store.hset(b"users:1", &[(b"id".as_slice(), b"1".as_slice()), (b"email", b"a@b")]).unwrap();
    cmds.load_snapshot_aux(None, false);
    cmds.on_shard_tick(&mut store);
    assert!(store.is_packed(b"users:1"));
}
