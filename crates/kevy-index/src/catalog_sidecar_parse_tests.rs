//! Sidecar lines the reader must take apart column by column: an ANN
//! index's parameters, and each shared column that can be malformed.

use super::*;

fn line_of(c: &Catalog) -> (String, String) {
    let text = c.to_sidecar();
    let mut lines = text.lines();
    let header = lines.next().expect("a header").to_string();
    (header, lines.next().expect("one index").to_string())
}

#[test]
fn an_ann_index_reads_back_with_every_parameter() {
    let ann = AnnSpec::new(3).with_distance(1).with_m(8).with_ef(50);
    let spec = IndexSpec::builder("emb", "doc:", IndexKind::Ann, ValType::Vector)
        .with_field("v")
        .with_ann(ann)
        .build()
        .unwrap();
    let mut c = Catalog::new();
    c.create(spec).unwrap();
    let (_, line) = line_of(&c);
    assert!(line.ends_with("\t3,1,8,50"), "{line}");
    let back = Catalog::from_sidecar(&c.to_sidecar()).expect("an ann line loads");
    let got = back.iter().next().expect("the index").0.ann();
    assert_eq!(got.map(|a| (a.dim, a.distance, a.m, a.ef)), Some((3, 1, 8, 50)));
}

#[test]
fn an_ann_column_with_a_missing_or_unreadable_number_is_refused() {
    let spec = IndexSpec::builder("emb", "doc:", IndexKind::Ann, ValType::Vector)
        .with_field("v")
        .with_ann(AnnSpec::new(3))
        .build()
        .unwrap();
    let mut c = Catalog::new();
    c.create(spec).unwrap();
    let (header, line) = line_of(&c);
    let (head, col) = line.rsplit_once('\t').unwrap();
    assert_eq!(col, "3,0,16,200");
    for bad in ["3,0,16", "x,0,16,200", "3,x,16,200", "3,0,x,200", "3,0,16,x", "3,0,16,200,1"] {
        let text = format!("{header}\n{head}\t{bad}\n");
        assert!(Catalog::from_sidecar(&text).is_none(), "{bad}");
    }
}

#[test]
fn a_malformed_shared_column_refuses_the_sidecar() {
    let good = ["idx", "user:", "age:1", "i64", "range", "0"];
    let text = |cols: &[&str]| format!("kevy-index-catalog v4\n{}\n", cols.join("\t"));
    assert!(Catalog::from_sidecar(&text(&good)).is_some());
    for (at, bad) in [(0, "%zz"), (1, "%z"), (2, "age:heavy"), (3, "i65"), (5, "lots")] {
        let mut cols = good;
        cols[at] = bad;
        assert!(Catalog::from_sidecar(&text(&cols)).is_none(), "column {at} = {bad}");
    }
}
