use std::sync::atomic::Ordering;

use super::*;
use crate::RuntimeState;
use kevy_index::{Catalog, IndexKind, IndexValue, ValType};

fn spec(name: &str) -> IndexSpec {
    IndexSpec {
        name: name.into(),
        prefix: b"user:".to_vec(),
        fields: vec![kevy_index::FieldSpec::new(b"age".to_vec())],
        ty: ValType::I64,
        kind: IndexKind::Range,
        ann: None,
        max_bytes: 0,
        group_by: None,
        with_positions: false,
        values: Vec::new(),
        composite: None,
    }
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
    assert!(segment_building(&ctx, &mut store, b"t_age"));
    assert!(with_ready_segment(&ctx, &mut store, b"t_age", |_, _| ()).is_err());

    // user:3 has no hash yet — create it and write again (HSET path).
    store.hset(b"user:3", &[(b"age".as_slice(), b"40".as_slice())]).unwrap();
    on_write(&ctx, &mut store, b"user:3");

    // Tick drains the backfill.
    on_tick(&ctx, &mut store);
    let (hits, stats) = with_ready_segment(&ctx, &mut store, b"t_age", |spec, seg| {
        let min = IndexValue::parse_literal(spec.ty, b"0").unwrap();
        let max = IndexValue::parse_literal(spec.ty, b"100").unwrap();
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
    let hits = with_ready_segment(&ctx, &mut store, b"t_age", |spec, seg| {
        let min = IndexValue::parse_literal(spec.ty, b"0").unwrap();
        let max = IndexValue::parse_literal(spec.ty, b"100").unwrap();
        seg.range(&min, &max, None, 10).0
    })
    .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits.last().unwrap().0, b"user:1".to_vec());
    assert_eq!(hits.last().unwrap().1, IndexValue::I64(99));

    ctx.state.install_index_catalog(Catalog::new());
    assert!(!ctx.state.catalogs.index_nonempty());
}
