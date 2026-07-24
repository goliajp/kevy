//! Sidecar round-trip tests, split from `catalog_sidecar.rs` for the
//! 500-LOC house rule (a `#[path]` child, so the codec's private helpers
//! stay reachable).

mod sidecar_v2_tests {
    use super::super::*;
    use crate::IndexState;

    fn spec(name: &str) -> IndexSpec {
        IndexSpec::single_field(
            name.into(),
            b"user:".to_vec(),
            b"age".to_vec(),
            ValType::I64,
            IndexKind::Range,
        )
    }

    /// The point of the version bump: a sidecar written before
    /// multi-field must keep loading forever. A catalog that refuses to
    /// parse is not an error the operator sees -- it is every index
    /// silently rebuilding from scratch on the next boot.
    #[test]
    fn a_v1_sidecar_still_loads() {
        let v1 = "kevy-index-catalog v1\nidx\tuser:\tage\ti64\trange\t0\n";
        let c = Catalog::from_sidecar(v1).expect("v1 must stay readable");
        let got: Vec<_> = c.iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.field(), b"age");
        assert_eq!(got[0].0.fields.len(), 1, "a v1 field is the one-element case");
        assert_eq!(got[0].0.fields[0].weight, 1.0, "and it is neutrally weighted");
    }

    /// The format must be able to carry weighted multi-field specs
    /// NOW, so that lifting the engine gate later is not also a disk
    /// format change. Only the writing half is checked: `create`
    /// refuses multi-field today, and `from_sidecar` goes through it,
    /// so no such sidecar can exist to read back yet.
    #[test]
    fn v3_serialises_several_weighted_fields() {
        let mut s = spec("multi");
        s.fields = vec![
            FieldSpec { name: b"title".to_vec(), weight: 3.0 },
            FieldSpec { name: b"body".to_vec(), weight: 1.0 },
        ];
        let mut c = Catalog::new();
        c.specs.push((s, IndexState::Building));
        let text = c.to_sidecar();
        // The writer always emits the newest version; what this test
        // pins is the weighted-field column, which v4 did not touch.
        assert!(text.starts_with("kevy-index-catalog v4"));
        let col3 = text.lines().nth(1).unwrap().split('\t').nth(2).unwrap();
        assert_eq!(col3, "title:3,body:1");
    }

    /// A v2 sidecar written before positions must keep loading — same
    /// forever-readable contract that protects v1.
    #[test]
    fn a_v2_sidecar_still_loads() {
        let v2 = "kevy-index-catalog v2\nidx\tuser:\ttitle:3,body:1\tstr\ttext\t0\n";
        let c = Catalog::from_sidecar(v2).expect("v2 must stay readable");
        let got: Vec<_> = c.iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.fields.len(), 2);
        assert!(!got[0].0.with_positions, "a v2 text index has no positions flag");
    }

    /// A text index created WITH POSITIONS round-trips the flag through
    /// the v3 `pos` column; a text index without it does not, and neither
    /// grows a spurious 7th column that would collide with ann/agg.
    #[test]
    fn positions_flag_round_trips_on_v3() {
        let mut with = IndexSpec::single_field(
            b"phrase".into(),
            b"doc:".to_vec(),
            b"body".to_vec(),
            ValType::Str,
            IndexKind::Text,
        );
        with.with_positions = true;
        let plain = IndexSpec::single_field(
            b"plain".into(),
            b"doc:".to_vec(),
            b"body".to_vec(),
            ValType::Str,
            IndexKind::Text,
        );
        let mut c = Catalog::new();
        c.create(with).unwrap();
        c.create(plain).unwrap();
        let text = c.to_sidecar();
        // The positions index carries a 7th `pos` column; the plain one
        // stays at six.
        let lines: Vec<&str> = text.lines().skip(1).collect();
        let pos_line = lines.iter().find(|l| l.starts_with("phrase")).unwrap();
        let plain_line = lines.iter().find(|l| l.starts_with("plain")).unwrap();
        assert_eq!(pos_line.split('\t').count(), 7);
        assert_eq!(pos_line.split('\t').nth(6), Some("pos"));
        assert_eq!(plain_line.split('\t').count(), 6);

        let back = Catalog::from_sidecar(&text).expect("v3 round trip");
        let specs: Vec<_> = back.iter().map(|(s, _)| s.clone()).collect();
        let with_back = specs.iter().find(|s| s.name == b"phrase").unwrap();
        let plain_back = specs.iter().find(|s| s.name == b"plain").unwrap();
        assert!(with_back.with_positions, "positions flag survives the round trip");
        assert!(!plain_back.with_positions, "plain text index stays position-free");
    }

    /// A field name containing the separators must survive, or a
    /// perfectly legal hash field silently corrupts the catalog.
    #[test]
    fn separators_in_a_field_name_survive() {
        let mut s = spec("esc");
        s.fields = vec![FieldSpec::new(b"we:ird,name\there".to_vec())];
        let mut c = Catalog::new();
        c.create(s).unwrap();
        let back = Catalog::from_sidecar(&c.to_sidecar()).expect("escaped round trip");
        let got: Vec<_> = back.iter().collect();
        assert_eq!(got[0].0.field(), b"we:ird,name\there");
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed() {
        assert!(Catalog::from_sidecar("kevy-index-catalog v9\n").is_none());
    }
}


mod sidecar_v4_tests {
    use super::super::*;

    fn text_spec(name: &str, positions: bool, values: &[&str]) -> IndexSpec {
        IndexSpec {
            name: name.into(),
            prefix: b"a:".to_vec(),
            fields: vec![FieldSpec::new(b"body".to_vec())],
            ty: ValType::Str,
            kind: IndexKind::Text,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: positions,
            values: values.iter().map(|v| ValueSpec::new(v.as_bytes().to_vec())).collect(),
        }
    }

    fn round_trip(specs: Vec<IndexSpec>) -> Vec<IndexSpec> {
        let mut c = Catalog::new();
        for s in specs {
            c.create(s).expect("valid spec");
        }
        let text = c.to_sidecar();
        Catalog::from_sidecar(&text)
            .expect("round trip")
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    #[test]
    fn declared_values_survive_the_round_trip() {
        let back = round_trip(vec![
            text_spec("plain", false, &[]),
            text_spec("pos_only", true, &[]),
            text_spec("vals_only", false, &["price", "status"]),
            text_spec("both", true, &["price"]),
        ]);
        assert_eq!(back[0].values, Vec::<ValueSpec>::new());
        assert!(!back[0].with_positions);
        assert!(back[1].with_positions && back[1].values.is_empty());
        assert!(!back[2].with_positions);
        assert_eq!(back[2].values, vec![ValueSpec::new(b"price".to_vec()), ValueSpec::new(b"status".to_vec())]);
        assert!(back[3].with_positions);
        assert_eq!(back[3].values, vec![ValueSpec::new(b"price".to_vec())]);
    }

    /// A field name holding the column's own separators must not split
    /// into two — the same hazard the weighted-field column escapes for.
    #[test]
    fn separators_inside_a_value_name_survive() {
        let back = round_trip(vec![text_spec("odd", false, &["a,b", "c:d"])]);
        assert_eq!(
            back[0].values,
            vec![ValueSpec::new(b"a,b".to_vec()), ValueSpec::new(b"c:d".to_vec())]
        );
    }

    /// A positions-only index still writes exactly the v3 column, so the
    /// shape only changes for an index that uses the new capability.
    #[test]
    fn positions_only_keeps_the_v3_column() {
        let mut c = Catalog::new();
        c.create(text_spec("pos_only", true, &[])).unwrap();
        let text = c.to_sidecar();
        assert!(text.starts_with("kevy-index-catalog v4"));
        assert!(text.lines().nth(1).unwrap().ends_with("\tpos"), "{text}");
    }

    /// v3 sidecars keep loading: one that predates VALUES simply has no
    /// declared ones, and a refusing sidecar is an index that silently
    /// rebuilds from scratch.
    #[test]
    fn a_v3_sidecar_still_loads() {
        let v3 = "kevy-index-catalog v3\nold\ta:\tbody:1\tstr\ttext\t0\tpos\n";
        let got: Vec<IndexSpec> =
            Catalog::from_sidecar(v3).expect("v3 loads").iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].with_positions, "the v3 flag still means positions");
        assert!(got[0].values.is_empty(), "a v3 index declares no values");
    }

    /// VALUES rides the kinds with a stored-value column (text, range,
    /// unique — the capacity arc's G1); ann and agg carry none, so
    /// accepting the declaration there would store nothing and filter
    /// on nothing.
    #[test]
    fn values_on_ann_or_agg_is_refused_scalar_kinds_accept() {
        for kind in [IndexKind::Range, IndexKind::Unique] {
            let mut s = text_spec("ok", false, &["price"]);
            s.kind = kind;
            s.ty = ValType::I64;
            assert!(Catalog::new().create(s).is_ok(), "VALUES on {kind:?}");
        }
        let mut s = text_spec("agg", false, &["price"]);
        s.kind = IndexKind::Agg;
        s.ty = ValType::I64;
        s.group_by = Some(b"g".to_vec());
        assert_eq!(
            Catalog::new().create(s),
            Err("ERR VALUES requires KIND text|range|unique")
        );
    }
}

mod sidecar_v5_tests {
    use super::super::*;

    fn scalar_spec(name: &str, kind: IndexKind, values: &[(&str, ValType)]) -> IndexSpec {
        let mut s = IndexSpec::single_field(
            name.into(),
            b"user:".to_vec(),
            b"age".to_vec(),
            ValType::I64,
            kind,
        );
        s.values = values
            .iter()
            .map(|(n, ty)| ValueSpec { name: n.as_bytes().to_vec(), ty: *ty })
            .collect();
        s
    }

    /// A5 — the acceptance criterion, as serialized bytes: a catalog
    /// without scalar-kind VALUES must serialize EXACTLY as the pre-G1
    /// writer did (v4 header, six columns), so a store that never
    /// declares the capability is byte-identical on disk.
    #[test]
    fn a5_no_values_catalog_serializes_byte_identically_to_v4() {
        let mut c = Catalog::new();
        c.create(scalar_spec("byage", IndexKind::Range, &[])).unwrap();
        c.create(scalar_spec("uniq", IndexKind::Unique, &[])).unwrap();
        assert_eq!(
            c.to_sidecar(),
            "kevy-index-catalog v4\n\
             byage\tuser:\tage:1\ti64\trange\t0\n\
             uniq\tuser:\tage:1\ti64\tunique\t0\n",
            "the exact pre-change writer output"
        );
    }

    /// Even a text index WITH values keeps the v4 header — v5 exists
    /// only for the scalar-kind column, so downgrade compatibility is
    /// surrendered only by stores that use the new capability.
    #[test]
    fn text_values_alone_do_not_bump_the_header() {
        let mut s = scalar_spec("t", IndexKind::Text, &[("price", ValType::F64)]);
        s.ty = ValType::Str;
        let mut c = Catalog::new();
        c.create(s).unwrap();
        assert!(c.to_sidecar().starts_with("kevy-index-catalog v4\n"));
    }

    #[test]
    fn scalar_values_round_trip_through_v5() {
        let mut c = Catalog::new();
        c.create(scalar_spec(
            "byage",
            IndexKind::Range,
            &[("city", ValType::Str), ("price", ValType::F64)],
        ))
        .unwrap();
        c.create(scalar_spec("plain", IndexKind::Unique, &[])).unwrap();
        let text = c.to_sidecar();
        assert!(text.starts_with("kevy-index-catalog v5\n"), "{text}");
        let byage_line = text.lines().nth(1).unwrap();
        assert_eq!(byage_line.split('\t').nth(6), Some("-,city:str,price:f64"));
        assert_eq!(
            text.lines().nth(2).unwrap().split('\t').count(),
            6,
            "an undeclared index keeps its six columns even inside a v5 file"
        );

        let back = Catalog::from_sidecar(&text).expect("v5 round trip");
        let (spec, _) = back.get(b"byage").expect("index");
        assert_eq!(spec.kind, IndexKind::Range);
        assert_eq!(
            spec.values,
            vec![
                ValueSpec { name: b"city".to_vec(), ty: ValType::Str },
                ValueSpec { name: b"price".to_vec(), ty: ValType::F64 },
            ]
        );
        assert!(back.get(b"plain").expect("index").0.values.is_empty());
    }

    /// A `pos` head on a scalar line is malformed, not ignored —
    /// positions stay text-only.
    #[test]
    fn a_positions_head_on_a_scalar_line_is_refused() {
        let bad = "kevy-index-catalog v5\nidx\tuser:\tage:1\ti64\trange\t0\tpos,city:str\n";
        assert!(Catalog::from_sidecar(bad).is_none());
    }

    /// A scalar 7th column before v5 never existed on disk; refusing it
    /// keeps the version headers honest.
    #[test]
    fn a_scalar_values_column_under_a_v4_header_is_refused() {
        let bad = "kevy-index-catalog v4\nidx\tuser:\tage:1\ti64\trange\t0\t-,city:str\n";
        assert!(Catalog::from_sidecar(bad).is_none());
    }
}

mod value_type_tests {
    use super::super::*;

    /// A declared type survives the sidecar, so a numeric filter still
    /// compares as a number after a restart. Guessing per query would
    /// make the answer depend on the data.
    #[test]
    fn value_types_round_trip() {
        let mut c = Catalog::new();
        c.create(IndexSpec {
            name: b"t".to_vec(),
            prefix: b"a:".to_vec(),
            fields: vec![FieldSpec::new(b"body".to_vec())],
            ty: ValType::Str,
            kind: IndexKind::Text,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: false,
            values: vec![
                ValueSpec { name: b"price".to_vec(), ty: ValType::F64 },
                ValueSpec { name: b"rank".to_vec(), ty: ValType::I64 },
                ValueSpec::new(b"status".to_vec()),
            ],
        })
        .unwrap();
        let back = Catalog::from_sidecar(&c.to_sidecar()).expect("round trip");
        let (spec, _) = back.get(b"t").expect("index");
        assert_eq!(
            spec.values,
            vec![
                ValueSpec { name: b"price".to_vec(), ty: ValType::F64 },
                ValueSpec { name: b"rank".to_vec(), ty: ValType::I64 },
                ValueSpec { name: b"status".to_vec(), ty: ValType::Str },
            ]
        );
    }
}
