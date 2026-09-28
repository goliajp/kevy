//! Global partitionings in the catalog and its sidecar.

use crate::catalog::{Catalog, IndexKind, IndexSpec, ValType, ValueSpec};
use crate::{Partitioning, partition_owner};

fn spec(name: &str, kind: IndexKind) -> IndexSpec {
    IndexSpec::single_field(name.into(), b"user:".to_vec(), b"age".to_vec(), ValType::I64, kind)
}

fn global(splits: &[&[u8]]) -> Partitioning {
    Partitioning::Global { splits: splits.iter().map(|s| s.to_vec()).collect() }
}

#[test]
fn a_catalog_with_global_indexes_round_trips_through_a_v7_sidecar() {
    let mut c = Catalog::new();
    c.create(spec("local", IndexKind::Range)).unwrap();
    c.create_with(spec("split", IndexKind::Range), global(&[b"\x80\x00", b"\x80\x10\xff"]))
        .unwrap();
    c.create_with(spec("one", IndexKind::Unique), global(&[])).unwrap();
    let mut valued = spec("valued", IndexKind::Range);
    valued.values = vec![ValueSpec { name: b"city".to_vec(), ty: ValType::Str }];
    c.create_with(valued, global(&[b"m"])).unwrap();
    let text = c.to_sidecar();
    assert!(text.starts_with("kevy-index-catalog v7\n"), "{text}");
    let back = Catalog::from_sidecar(&text).expect("v7 reads back");
    for name in ["local", "split", "one", "valued"] {
        assert_eq!(back.partitioning(name.as_bytes()), c.partitioning(name.as_bytes()), "{name}");
        assert_eq!(
            back.get(name.as_bytes()).map(|(s, _)| s),
            c.get(name.as_bytes()).map(|(s, _)| s)
        );
    }
    assert_eq!(back.to_sidecar(), text, "a second round trip changes nothing");
}

#[test]
fn a_catalog_without_global_indexes_writes_the_sidecar_it_always_did() {
    let mut with = Catalog::new();
    with.create_with(spec("local", IndexKind::Range), Partitioning::Local).unwrap();
    let mut plain = Catalog::new();
    plain.create(spec("local", IndexKind::Range)).unwrap();
    assert_eq!(with.to_sidecar(), plain.to_sidecar());
    assert!(plain.to_sidecar().starts_with("kevy-index-catalog v4\n"));
}

#[test]
fn a_global_partitioning_is_refused_where_it_cannot_apply() {
    let mut c = Catalog::new();
    let mut text = spec("t", IndexKind::Text);
    text.ty = ValType::Str;
    assert_eq!(
        c.create_with(text, global(&[])),
        Err("ERR PARTITION global requires KIND range|unique")
    );
    assert_eq!(
        c.create_with(spec("r", IndexKind::Range), global(&[b"b", b"a"])),
        Err("ERR SPLIT AT values must be strictly increasing")
    );
    assert_eq!(
        c.create_with(spec("r", IndexKind::Range), global(&[b"a", b"a"])),
        Err("ERR SPLIT AT values must be strictly increasing")
    );
    assert!(c.get(b"r").is_none(), "a refused index is not half-created");
}

#[test]
fn dropping_an_index_drops_its_partitioning_and_splits_can_move() {
    let mut c = Catalog::new();
    c.create_with(spec("g", IndexKind::Range), global(&[b"m"])).unwrap();
    assert!(c.set_splits(b"g", vec![b"f".to_vec(), b"t".to_vec()]));
    assert_eq!(c.partitioning(b"g").partitions(), 3);
    assert!(!c.set_splits(b"g", vec![b"t".to_vec(), b"f".to_vec()]), "out of order");
    assert!(!c.set_splits(b"absent", vec![]));
    assert!(c.drop_index(b"g"));
    c.create(spec("g", IndexKind::Range)).unwrap();
    assert_eq!(
        c.partitioning(b"g"),
        &Partitioning::Local,
        "a new index of the same name starts local"
    );
}

#[test]
fn every_value_has_one_partition_and_every_partition_one_owner() {
    let p = global(&[b"c", b"f"]);
    let at = |v: &[u8]| p.partition_of(v);
    assert_eq!([at(b""), at(b"b"), at(b"c"), at(b"e"), at(b"f"), at(b"zz")], [0, 0, 1, 1, 2, 2]);
    assert_eq!(Partitioning::Local.partition_of(b"anything"), 0);
    for n in [1, 3, 16, 64] {
        let mut owners: Vec<usize> = (0..n).map(|q| partition_owner(b"idx", q, n)).collect();
        owners.sort();
        assert_eq!(owners, (0..n).collect::<Vec<_>>(), "n = {n}");
    }
}
