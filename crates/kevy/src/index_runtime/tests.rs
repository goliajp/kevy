use super::*;
use kevy_index::{IndexKind, ValType};

fn spec(name: &str) -> IndexSpec {
    IndexSpec {
        name: name.into(),
        prefix: b"user:".to_vec(),
        field: b"age".to_vec(),
        ty: ValType::I64,
        kind: IndexKind::Range,
        ann: None,
        max_bytes: 0,
        group_by: None,
    }
}

fn install_one(epoch: &AtomicU64, name: &str) {
    let mut c = Catalog::new();
    c.create(spec(name)).unwrap();
    install_catalog(epoch, c);
}

#[test]
fn hook_backfill_and_query_lifecycle() {
    let epoch = AtomicU64::new(0);
    let shard = ShardCtx::default();
    let mut store = Store::new();
    // Pre-existing rows (to be backfilled).
    store.hset(b"user:1", &[(b"age".to_vec(), b"30".to_vec())]).unwrap();
    store.hset(b"user:2", &[(b"age".to_vec(), b"25".to_vec())]).unwrap();
    store.hset(b"user:bad", &[(b"age".to_vec(), b"x".to_vec())]).unwrap();
    install_one(&epoch, "t_age");
    assert_eq!(epoch.load(Ordering::Acquire), 1, "install bumps the control epoch");
    assert!(catalog_nonempty());

    // Live write during Building: hook double-writes.
    on_write(&shard, &mut store, b"user:3");
    assert!(segment_building(&shard, &mut store, b"t_age"));
    assert!(with_ready_segment(&shard, &mut store, b"t_age", |_, _| ()).is_err());

    // user:3 has no hash yet — create it and write again (HSET path).
    store.hset(b"user:3", &[(b"age".to_vec(), b"40".to_vec())]).unwrap();
    on_write(&shard, &mut store, b"user:3");

    // Tick drains the backfill.
    on_tick(&shard, &mut store);
    let (hits, stats) = with_ready_segment(&shard, &mut store, b"t_age", |spec, seg| {
        let min = IndexValue::parse_literal(spec.ty, b"0").unwrap();
        let max = IndexValue::parse_literal(spec.ty, b"100").unwrap();
        (seg.range(&min, &max, None, 10).0, seg.stats())
    })
    .unwrap();
    assert_eq!(hits.len(), 3, "2 backfilled + 1 live");
    assert_eq!(hits[0].0, b"user:2".to_vec());
    assert_eq!(stats.coerce_failures, 1, "user:bad excluded");

    // Update moves the row; delete removes it.
    store.hset(b"user:1", &[(b"age".to_vec(), b"99".to_vec())]).unwrap();
    on_write(&shard, &mut store, b"user:1");
    store.del(&[b"user:2".to_vec()]);
    on_write(&shard, &mut store, b"user:2");
    let hits = with_ready_segment(&shard, &mut store, b"t_age", |spec, seg| {
        let min = IndexValue::parse_literal(spec.ty, b"0").unwrap();
        let max = IndexValue::parse_literal(spec.ty, b"100").unwrap();
        seg.range(&min, &max, None, 10).0
    })
    .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits.last().unwrap().0, b"user:1".to_vec());
    assert_eq!(hits.last().unwrap().1, IndexValue::I64(99));

    install_catalog(&epoch, Catalog::new()); // cleanup for other tests
    assert!(!catalog_nonempty());
}
