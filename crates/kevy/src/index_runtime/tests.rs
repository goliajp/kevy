use std::sync::atomic::Ordering;

use super::*;
use crate::RuntimeState;
use kevy_index::{Catalog, IndexKind, IndexValue, ValType};

fn spec(name: &str) -> IndexSpec {
    IndexSpec::builder(name, "user:", IndexKind::Range, ValType::I64)
        .with_field("age")
        .build()
        .unwrap()
}

fn install_one(state: &RuntimeState, name: &str) {
    let mut c = Catalog::new();
    c.create(spec(name)).unwrap();
    state.install_index_catalog(c);
}

#[test]
fn hook_backfill_and_query_lifecycle() {
    let cmds = crate::KevyCommands::new();
    let ctx = cmds.ctx();
    let mut store = Store::new();
    // Pre-existing rows (to be backfilled).
    store.hset(b"user:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
    store.hset(b"user:2", &[(b"age".as_slice(), b"25".as_slice())]).unwrap();
    store.hset(b"user:bad", &[(b"age".as_slice(), b"x".as_slice())]).unwrap();
    let epoch0 = ctx.state.control_epoch().load(Ordering::Acquire);
    install_one(ctx.state, "t_age");
    assert_eq!(
        ctx.state.control_epoch().load(Ordering::Acquire),
        epoch0 + 1,
        "install bumps the control epoch"
    );
    assert!(ctx.state.catalogs.index_nonempty());

    // Live write during Building: hook double-writes.
    on_write(&ctx, &mut store, b"user:3");
    assert!(segment_building(&ctx, b"t_age"));
    assert!(with_ready_segment(&ctx, b"t_age", |_, _, _| ()).is_err());

    // user:3 has no hash yet — create it and write again (HSET path).
    store.hset(b"user:3", &[(b"age".as_slice(), b"40".as_slice())]).unwrap();
    on_write(&ctx, &mut store, b"user:3");

    // Tick drains the backfill.
    on_tick(&ctx, &mut store);
    let (hits, stats) = with_ready_segment(&ctx, b"t_age", |spec, seg, _| {
        let min = IndexValue::parse_literal(spec.ty(), b"0").unwrap();
        let max = IndexValue::parse_literal(spec.ty(), b"100").unwrap();
        (seg.range(&min, &max, None, 10).0, seg.stats())
    })
    .unwrap();
    assert_eq!(hits.len(), 3, "2 backfilled + 1 live");
    assert_eq!(hits[0].0, b"user:2".to_vec());
    assert_eq!(stats.coerce_failures, 1, "user:bad excluded");

    // Update moves the row; delete removes it.
    store.hset(b"user:1", &[(b"age".as_slice(), b"99".as_slice())]).unwrap();
    on_write(&ctx, &mut store, b"user:1");
    store.del(&[b"user:2".as_slice()]);
    on_write(&ctx, &mut store, b"user:2");
    let hits = with_ready_segment(&ctx, b"t_age", |spec, seg, _| {
        let min = IndexValue::parse_literal(spec.ty(), b"0").unwrap();
        let max = IndexValue::parse_literal(spec.ty(), b"100").unwrap();
        seg.range(&min, &max, None, 10).0
    })
    .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits.last().unwrap().0, b"user:1".to_vec());
    assert_eq!(hits.last().unwrap().1, IndexValue::I64(99));

    ctx.state.install_index_catalog(Catalog::new());
    assert!(!ctx.state.catalogs.index_nonempty());
}

/// `IDX.REBUILD` and the vector query answer an index's state: still
/// building before its first tick, over its budget once the build breaks
/// `MAXMEM`, and not a vector index or no index at all by name.
#[test]
fn rebuild_and_knn_answer_a_building_an_over_budget_and_a_missing_index() {
    use super::test_shard::{Shard, text};
    let mut s = Shard::new();
    for i in 0..200 {
        s.hset(&format!("user:{i}"), &[("age", &i.to_string())]);
    }
    s.ok("IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range");
    assert!(text(&s.ext("IDX.REBUILD age")).starts_with("-INDEXBUILDING"), "before a tick");
    // the origin routes only `IDX.REBUILD <name>` here; a shard handed less says so
    assert!(text(&s.ext("IDX.REBUILD")).contains("bad arguments"));
    s.ok("IDX.CREATE tiny ON PREFIX user: FIELD age TYPE i64 KIND range MAXMEM 64");
    s.settle();
    assert_eq!(text(&s.ext("IDX.REBUILD age")), "+OK\r\n");
    assert!(text(&s.ext("IDX.REBUILD tiny")).starts_with("-INDEXOVERBUDGET"));
    let knn = |s: &mut Shard, name: &str| text(&s.ext(&format!("IDX.QUERY {name} KNN csv:1,2,3")));
    assert!(knn(&mut s, "tiny").starts_with("-INDEXOVERBUDGET"), "{}", knn(&mut s, "tiny"));
    assert!(knn(&mut s, "age").starts_with("-ERR"), "{}", knn(&mut s, "age"));
    assert!(knn(&mut s, "nope").starts_with("-ERR no such index"), "{}", knn(&mut s, "nope"));
}

/// The shard tick reads the index gate before the catalog, so a drop of
/// the last index can land in between.
#[test]
fn a_tick_after_the_last_index_dropped_releases_the_shard_segments() {
    let cmds = crate::KevyCommands::new();
    let ctx = cmds.ctx();
    let mut store = Store::new();
    store.hset(b"user:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
    install_one(ctx.state, "t_age");
    on_tick(&ctx, &mut store);
    let entries = with_ready_segment(&ctx, b"t_age", |_, seg, _| seg.stats().entries);
    assert_eq!(entries.unwrap(), 1);
    ctx.state.install_index_catalog(Catalog::new());
    on_tick(&ctx, &mut store);
    assert!(ctx.shard.indexes.borrow().idx.is_empty());
    assert!(with_ready_segment(&ctx, b"t_age", |_, _, _| ()).is_err());
}
