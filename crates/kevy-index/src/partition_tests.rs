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

/// Largest partition over the mean, for `population` cut by `splits`.
fn max_over_mean(population: &[Vec<u8>], splits: Vec<Vec<u8>>) -> f64 {
    let part = Partitioning::Global { splits };
    let mut sizes = vec![0usize; part.partitions()];
    for v in population {
        sizes[part.partition_of(v)] += 1;
    }
    let max = *sizes.iter().max().unwrap() as f64;
    max / (population.len() as f64 / sizes.len() as f64)
}

#[test]
fn quantiles_of_a_strided_sample_cut_even_partitions() {
    // 100_000 distinct values in a scrambled order, as a key walk sees them
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let population: Vec<Vec<u8>> = (0..100_000u64)
        .map(|_| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (x >> 16).to_be_bytes().to_vec()
        })
        .collect();
    // 512 samples per partition: a partition's share then varies by about
    // 1/sqrt(512) ≈ 4.4%, and the largest of 16 lands near 2σ
    for parts in [2, 4, 8, 16] {
        let stride = population.len() / (512 * parts);
        let sample: Vec<Vec<u8>> = population.iter().step_by(stride).cloned().collect();
        let splits = crate::splits_from_sample(sample, parts);
        assert_eq!(splits.len(), parts - 1);
        let skew = max_over_mean(&population, splits);
        assert!(skew < 1.1, "P={parts}: largest partition {skew:.3}× the mean");
    }
}

#[test]
fn a_value_holding_more_than_its_share_is_not_split() {
    // half the rows share one value: it stays whole, the rest still splits
    let mut sample: Vec<Vec<u8>> = vec![vec![50]; 500];
    sample.extend((0..500u32).map(|v| vec![(v % 100) as u8]));
    let splits = crate::splits_from_sample(sample, 4);
    assert!(splits.windows(2).all(|w| w[0] < w[1]));
    assert!(splits.len() < 3, "{splits:?}");
}

#[test]
fn a_store_whose_paths_are_all_local_refuses_global_by_name() {
    let argv: [&[u8]; 12] = [
        b"TABLE.DECLARE",
        b"u",
        b"PREFIX",
        b"u:",
        b"PK",
        b"id",
        b"COLUMN",
        b"id",
        b"i64",
        b"INDEX",
        b"id",
        b"range",
    ];
    let mut global = argv.to_vec();
    global.push(b"GLOBAL");
    let err = crate::parse_table_declare(&global).unwrap_err();
    assert!(err.contains("GLOBAL is a server feature"), "{err}");
    assert!(crate::parse_table_declare(&argv).is_ok());
    // a bare SPLIT without AT is not the grammar
    global.extend_from_slice(&[b"SPLIT".as_slice(), b"5"]);
    assert!(crate::parse_table_declare_partitioned(&global).unwrap_err().contains("usage"));
}

#[test]
fn an_orderpath_split_point_reads_back_from_its_hex() {
    let t = crate::parse_table_declare(&[
        b"TABLE.DECLARE",
        b"u",
        b"PREFIX",
        b"u:",
        b"PK",
        b"id",
        b"COLUMN",
        b"id",
        b"i64",
        b"COLUMN",
        b"city",
        b"str",
        b"ORDERPATH",
        b"by_city",
        b"ON",
        b"city",
        b"THEN",
        b"id",
    ])
    .unwrap();
    let spec = crate::compile_table(&t).unwrap().into_iter().find(|s| s.name == b"u.by_city");
    let spec = spec.expect("the orderpath compiles");
    let point = vec![0x00, 0x61, 0xff, 0x10];
    let text = crate::split_point_text(&spec, &point);
    assert_eq!(text, b"0x0061ff10");
    assert_eq!(crate::parse_split_point(&spec, &text), Some(point));
    for bad in [&b"0061ff10"[..], b"0x061", b"0xzz"] {
        assert_eq!(crate::parse_split_point(&spec, bad), None, "{}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn a_split_point_reads_back_in_its_column_type() {
    for (ty, raw) in [(ValType::F64, &b"-2.5"[..]), (ValType::Str, b"tokyo"), (ValType::I64, b"-7")]
    {
        let s = IndexSpec::single_field(
            b"i".to_vec(),
            b"u:".to_vec(),
            b"f".to_vec(),
            ty,
            IndexKind::Range,
        );
        let enc = crate::parse_split_point(&s, raw).unwrap();
        assert_eq!(crate::split_point_text(&s, &enc), raw);
        let part = Partitioning::Global { splits: vec![enc] };
        assert_eq!(part.split_values(ty), [raw.to_vec()]);
    }
}

#[test]
fn a_sidecar_with_a_partition_column_it_cannot_read_is_refused() {
    let mut c = Catalog::new();
    c.create_with(spec("g", IndexKind::Range), global(&[b"\x80"])).unwrap();
    let text = c.to_sidecar();
    assert!(Catalog::from_sidecar(&text).is_some());
    for (from, to) in [("\tg,80", "\tq,80"), ("\tg,80", "\tg,8"), ("\tg,80", "\tg,zz")] {
        assert!(text.contains(from), "{text}");
        assert!(Catalog::from_sidecar(&text.replace(from, to)).is_none(), "{to}");
    }
}
