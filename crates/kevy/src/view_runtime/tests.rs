//! A materialized view on a shard holds what its indexes say, whatever
//! order the catalog, the rows and the builds reach the shard in.

use kevy_rt::{Argv, Commands};
use kevy_store::Store;

use crate::KevyCommands;

fn run(cmds: &KevyCommands, store: &mut Store, cmd: &str) -> Vec<u8> {
    let argv = Argv::from(cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
    cmds.dispatch(store, &argv)
}

fn age(store: &mut Store, key: &str, age: u32) {
    store.hset(key.as_bytes(), &[(b"age".as_slice(), age.to_string().as_bytes())]).unwrap();
}

/// The declarations the tests share: an index on `user:` ages and a
/// materialized view of the adults.
fn declare(cmds: &KevyCommands, store: &mut Store) {
    let index = "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range";
    assert_eq!(run(cmds, store, index), b"+OK\r\n");
    let view = "VIEW.CREATE adults QUERY age RANGE 18 200 ORDER BY age MODE materialized";
    assert_eq!(run(cmds, store, view), b"+OK\r\n");
}

/// The members the view answers with, once every build has finished.
fn members(cmds: &KevyCommands, store: &mut Store) -> Vec<String> {
    for _ in 0..16 {
        cmds.on_shard_tick(store);
    }
    let page = super::shard_page(&cmds.ctx(), b"adults", None, usize::MAX).unwrap();
    page.into_iter().map(|(_, k)| String::from_utf8(k).unwrap()).collect()
}

/// A replica's shard learns the catalog from another shard's full sync,
/// builds its view over its still empty keyspace, then loads its own
/// snapshot: the loader replaces the keyspace without the write hook.
#[test]
fn a_view_takes_in_the_rows_a_snapshot_load_puts_in_after_it_was_built() {
    let (cmds, mut store) = (KevyCommands::new(), Store::new());
    declare(&cmds, &mut store);
    assert_eq!(members(&cmds, &mut store), Vec::<String>::new());
    store.flushall();
    age(&mut store, "user:18", 18);
    age(&mut store, "user:5", 5);
    assert_eq!(members(&cmds, &mut store), ["user:18"]);
}

/// A member removed without the write hook (the loader, an expiry) leaves
/// the view with it.
#[test]
fn a_view_lets_go_of_a_row_removed_without_the_write_hook() {
    let (cmds, mut store) = (KevyCommands::new(), Store::new());
    age(&mut store, "user:18", 18);
    age(&mut store, "user:19", 19);
    declare(&cmds, &mut store);
    assert_eq!(members(&cmds, &mut store), ["user:18", "user:19"]);
    store.del(&[b"user:18".as_slice()]);
    assert_eq!(members(&cmds, &mut store), ["user:19"]);
}

/// A view declared while its index backfills is built once the index is
/// ready, not from the part the backfill has reached.
#[test]
fn a_view_is_built_once_its_index_is_ready() {
    let (cmds, mut store) = (KevyCommands::new(), Store::new());
    // more rows than one tick's backfill batch
    for i in 0..5000 {
        age(&mut store, &format!("user:{i}"), 20);
    }
    declare(&cmds, &mut store);
    assert_eq!(members(&cmds, &mut store).len(), 5000);
}

/// An index a view reads, dropped and declared again, is built again; a
/// write while it builds does not take its row out of the view.
#[test]
fn a_view_follows_an_index_built_again_under_it() {
    let (cmds, mut store) = (KevyCommands::new(), Store::new());
    age(&mut store, "user:18", 18);
    age(&mut store, "user:19", 19);
    declare(&cmds, &mut store);
    // another index keeps the shard's index hooks running across the drop
    let other = "IDX.CREATE other ON PREFIX user: FIELD age TYPE i64 KIND range";
    assert_eq!(run(&cmds, &mut store, other), b"+OK\r\n");
    assert_eq!(members(&cmds, &mut store), ["user:18", "user:19"]);
    assert_eq!(run(&cmds, &mut store, "IDX.DROP age"), b":1\r\n");
    cmds.on_shard_tick(&mut store);
    let index = "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range";
    assert_eq!(run(&cmds, &mut store, index), b"+OK\r\n");
    age(&mut store, "user:19", 30);
    cmds.on_write(&mut store, b"user:19");
    assert_eq!(members(&cmds, &mut store), ["user:18", "user:19"]);
}

/// FLUSHALL, then a write through the hook: the view still reads the
/// index it was reading.
#[test]
fn a_view_reads_its_index_after_a_flush() {
    let (cmds, mut store) = (KevyCommands::new(), Store::new());
    age(&mut store, "user:18", 18);
    declare(&cmds, &mut store);
    assert_eq!(members(&cmds, &mut store), ["user:18"]);
    assert_eq!(run(&cmds, &mut store, "FLUSHALL"), b"+OK\r\n");
    age(&mut store, "user:20", 20);
    cmds.on_write(&mut store, b"user:20");
    assert_eq!(members(&cmds, &mut store), ["user:20"]);
}
