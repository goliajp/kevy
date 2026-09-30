//! The index and view verbs on the embedded wire face: every option the
//! IDX.CREATE grammar names, the KNN / HYBRID / COMPOSE query shapes, and
//! the refusals VIEW.CREATE answers in the server's words.

use crate::{Config, Store};

use super::idx::spec_of;

fn store() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).expect("open in-memory store")
}

fn argv(s: &Store, argv: &[&[u8]]) -> String {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    super::dispatch(s, &owned, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn run(s: &Store, cmd: &str) -> String {
    let words: Vec<&[u8]> = cmd.split(' ').map(str::as_bytes).collect();
    argv(s, &words)
}

fn err(msg: &str) -> String {
    format!("-{msg}\r\n")
}

fn vector(x: f32, y: f32) -> Vec<u8> {
    [x.to_le_bytes(), y.to_le_bytes()].concat()
}

/// Documents with a title and a two-dimensional vector: `doc:1` is red
/// and sits at the origin, `doc:2` is red and far, `doc:3` is blue and
/// near the origin; `doc:4` has a title and no vector.
fn documents(s: &Store) {
    for (key, title, x, y) in [
        (&b"doc:1"[..], &b"red apple"[..], 0.0, 0.0),
        (b"doc:2", b"red car", 9.0, 9.0),
        (b"doc:3", b"blue sky", 0.5, 0.0),
    ] {
        s.hset(key, &[(b"title", title), (b"vec", vector(x, y).as_slice())]).unwrap();
    }
    s.hset(b"doc:4", &[(b"title", b"green pear")]).unwrap();
}

#[test]
fn idx_create_declares_what_every_option_asks_for() {
    let s = store();
    let ok = "+OK\r\n";
    let text = "IDX.CREATE docs ON PREFIX doc: FIELDS title body WEIGHTS 2 0.5 TYPE str KIND text \
                WITH POSITIONS VALUES year TYPES i64 MAXMEM 1024";
    assert_eq!(run(&s, text), ok);
    let docs = spec_of(&s, b"docs").unwrap();
    let fields: Vec<(&[u8], f32)> =
        docs.fields().iter().map(|f| (f.name.as_slice(), f.weight)).collect();
    assert_eq!(fields, [(&b"title"[..], 2.0), (b"body", 0.5)]);
    assert!(docs.has_positions());
    assert_eq!(docs.values()[0].name, b"year");
    assert_eq!(docs.values()[0].ty, crate::IndexValType::I64);

    assert_eq!(run(&s, "IDX.CREATE plain ON PREFIX doc: FIELDS title TYPE str KIND text"), ok);
    let plain = spec_of(&s, b"plain").unwrap();
    assert!(!plain.has_positions());
    assert_eq!(plain.fields()[0].weight, 1.0);

    let agg = "IDX.CREATE sales ON PREFIX o: FIELD amt TYPE i64 KIND agg GROUPBY region";
    assert_eq!(run(&s, agg), ok);
    assert_eq!(spec_of(&s, b"sales").unwrap().group_by(), Some(&b"region"[..]));

    for (name, distance, code) in [("near", "cosine", 0), ("far", "l2", 1), ("dot", "ip", 2)] {
        let ann = format!(
            "IDX.CREATE {name} ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 2 \
             DISTANCE {distance} M 8 EF 32 PARTITION local"
        );
        assert_eq!(run(&s, &ann), ok, "{ann}");
        let a = spec_of(&s, name.as_bytes()).unwrap().ann().unwrap();
        assert_eq!((a.dim, a.distance, a.m, a.ef), (2, code, 8, 32), "{ann}");
    }
}

#[test]
fn idx_create_refuses_each_malformed_option_in_the_servers_words() {
    let s = store();
    let head = "IDX.CREATE t ON PREFIX p:";
    let usage = err(super::idx_create::CREATE_USAGE);
    let partition =
        "ERR PARTITION global is a server feature; an embedded store's indexes are local";
    let cases: &[(&str, &str)] = &[
        ("FIELDZ f TYPE i64 KIND range", usage.as_str()),
        ("FIELDS TYPE str KIND text MAXMEM 1", "ERR FIELDS needs at least one field name"),
        ("FIELDS a b WEIGHTS 1 TYPE str KIND text", "ERR WEIGHTS count must match FIELDS count"),
        ("FIELD f TYPE str KIND text WITH OFFSETS", "ERR WITH only accepts POSITIONS"),
        ("FIELD f TYPE i64 KIND range MAXMEM lots", "ERR MAXMEM must be an integer byte count"),
        ("FIELD v TYPE vector KIND ann DIM 0", "ERR DIM must be 1-65536"),
        ("FIELD v TYPE vector KIND ann DIM 2 M 2", "ERR M must be 4-64"),
        ("FIELD v TYPE vector KIND ann DIM 2 EF 8", "ERR EF must be 16-1024"),
        (
            "FIELD v TYPE vector KIND ann DIM 2 DISTANCE manhattan",
            "ERR DISTANCE must be cosine|l2|ip",
        ),
        ("FIELD f TYPE i64 KIND range SPLIT 10", partition),
        ("FIELD f TYPE i64 KIND range PARTITION global", partition),
        ("FIELD f TYPE i64 KIND range COLOR red", "ERR syntax error"),
    ];
    for (tail, want) in cases {
        let got = run(&s, &format!("{head} {tail}"));
        let want = if want.starts_with('-') { want.to_string() } else { err(want) };
        assert_eq!(got, want, "{tail}");
    }
    let empty_group: &[&[u8]] = &[
        b"IDX.CREATE",
        b"t",
        b"ON",
        b"PREFIX",
        b"p:",
        b"FIELD",
        b"f",
        b"TYPE",
        b"i64",
        b"KIND",
        b"agg",
        b"GROUPBY",
        b"",
    ];
    assert_eq!(argv(&s, empty_group), err("ERR GROUPBY requires a field"));
    assert!(run(&s, "IDX.LIST").starts_with("*0"), "a refused create declared something");
}

#[test]
fn knn_answers_the_nearest_rows_and_refuses_what_it_cannot_read() {
    let s = store();
    documents(&s);
    let ann = "IDX.CREATE emb ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 2 DISTANCE l2";
    assert_eq!(run(&s, ann), "+OK\r\n");
    let got = run(&s, "IDX.QUERY emb KNN csv:0,0 LIMIT 2 EF 32 FIELDS title");
    let (first, second) = (got.find("doc:1").unwrap(), got.find("doc:3").unwrap());
    assert!(first < second && !got.contains("doc:2"), "{got}");
    assert!(got.contains("red apple") && got.contains("blue sky"), "{got}");

    let bad = |name: &str| {
        err(&format!(
            "ERR IDX.QUERY '{name}': bad arguments — run COMMAND DOCS IDX.QUERY for the syntax"
        ))
    };
    assert_eq!(
        run(&s, "IDX.QUERY nope KNN csv:0,0"),
        err("ERR no such index 'nope' (IDX.LIST enumerates them)")
    );
    assert_eq!(run(&s, "IDX.QUERY emb KNN csv:0,0 EF 8"), bad("emb"));
    assert_eq!(run(&s, "IDX.QUERY emb KNN csv:0,0,0"), bad("emb"));
    // a range index reads no vector of any width, so the empty one parses
    // and the search itself refuses
    assert_eq!(run(&s, "IDX.CREATE t ON PREFIX doc: FIELD title TYPE str KIND range"), "+OK\r\n");
    assert!(argv(&s, &[b"IDX.QUERY", b"t", b"KNN", b""]).starts_with("-ERR"));
}

#[test]
fn hybrid_ranks_first_the_row_both_searches_found() {
    let s = store();
    documents(&s);
    assert_eq!(run(&s, "IDX.CREATE docs ON PREFIX doc: FIELD title TYPE str KIND text"), "+OK\r\n");
    let ann = "IDX.CREATE emb ON PREFIX doc: FIELD vec TYPE vector KIND ann DIM 2 DISTANCE l2";
    assert_eq!(run(&s, ann), "+OK\r\n");
    let got = run(
        &s,
        "IDX.QUERY HYBRID docs MATCH red emb KNN csv:0,0 LIMIT 3 RRFK 60 EF 32 FIELDS title",
    );
    let at = |k: &str| got.find(k).unwrap_or_else(|| panic!("{k} missing from {got}"));
    assert!(at("doc:1") < at("doc:2") && at("doc:1") < at("doc:3"), "{got}");
    assert!(got.contains("red apple"), "{got}");
    // doc:4 alone matches "pear" and doc:2 is the nearest to (9,9): each
    // tops one list, so their fused scores tie and the key orders them
    let tie = run(&s, "IDX.QUERY HYBRID docs MATCH pear emb KNN csv:9,9 LIMIT 2");
    let (two, four) = (tie.find("doc:2").unwrap(), tie.find("doc:4").unwrap());
    assert!(two < four && !tie.contains("doc:3") && !tie.contains("doc:1"), "{tie}");

    let no_such =
        |name: &str| err(&format!("ERR no such index '{name}' (IDX.LIST enumerates them)"));
    assert_eq!(run(&s, "IDX.QUERY HYBRID nope MATCH red emb KNN csv:0,0"), no_such("nope"));
    assert_eq!(run(&s, "IDX.QUERY HYBRID docs MATCH red nope KNN csv:0,0"), no_such("nope"));
    assert!(
        run(&s, "IDX.QUERY HYBRID docs MATCH red emb KNN csv:0").contains("'emb': bad arguments")
    );
    // the text index reads no vector, so the empty one parses and the
    // nearest-neighbour search refuses the index
    let text_as_ann: &[&[u8]] =
        &[b"IDX.QUERY", b"HYBRID", b"docs", b"MATCH", b"red", b"docs", b"KNN", b""];
    assert!(argv(&s, text_as_ann).starts_with("-ERR"));
}

#[test]
fn compose_intersects_two_ranges_and_refuses_a_malformed_side() {
    let s = store();
    for (i, (age, score)) in [(20, 5), (30, 9), (40, 1), (70, 7)].into_iter().enumerate() {
        let (age, score) = (age.to_string(), score.to_string());
        s.hset(
            format!("u:{i}").as_bytes(),
            &[(b"age", age.as_bytes()), (b"score", score.as_bytes())],
        )
        .unwrap();
    }
    for f in ["age", "score"] {
        let cmd = format!("IDX.CREATE {f} ON PREFIX u: FIELD {f} TYPE i64 KIND range");
        assert_eq!(run(&s, &cmd), "+OK\r\n");
    }
    let got = run(&s, "IDX.QUERY COMPOSE AND age RANGE 0 50 score EQ 9");
    assert!(got.contains("u:1") && !got.contains("u:0") && !got.contains("u:3"), "{got}");
    let got = run(&s, "IDX.QUERY COMPOSE OR age RANGE 60 80 score RANGE 0 1");
    assert!(got.contains("u:2") && got.contains("u:3") && !got.contains("u:1"), "{got}");

    for bad in [
        "IDX.QUERY COMPOSE AND",
        "IDX.QUERY COMPOSE AND nope RANGE 0 1 age RANGE 0 1",
        "IDX.QUERY COMPOSE AND age",
        "IDX.QUERY COMPOSE AND age RANGE x 1 score RANGE 0 1",
        "IDX.QUERY COMPOSE AND age RANGE 0 1",
    ] {
        assert_eq!(run(&s, bad), err("ERR bad IDX arguments"), "{bad}");
    }
}

#[test]
fn view_create_refuses_what_the_server_refuses() {
    let s = store();
    for i in 0..6 {
        s.hset(format!("u:{i}").as_bytes(), &[(b"age", format!("{}", 10 * i).as_bytes())]).unwrap();
    }
    assert_eq!(run(&s, "IDX.CREATE age ON PREFIX u: FIELD age TYPE i64 KIND range"), "+OK\r\n");
    let q = "VIEW.CREATE v QUERY age RANGE 0 100";
    let order_required = err("ERR ORDER BY <index> is required");
    let cases = [
        (
            "VIEW.CREATE v QUERY nope RANGE 0 1 ORDER BY age".to_string(),
            err("ERR view leaf references unknown index"),
        ),
        (format!("{q} SORT BY age"), order_required.clone()),
        (format!("{q} ORDER BY"), order_required),
        (format!("{q} ORDER BY nope"), err("ERR ORDER BY references unknown index")),
        (format!("{q} ORDER BY age TOPK 3"), err("ERR TOPK requires MODE materialized")),
    ];
    for (cmd, want) in &cases {
        assert_eq!(&run(&s, cmd), want, "{cmd}");
    }
    assert!(run(&s, &format!("{q} ORDER BY age MODE sideways")).starts_with("-ERR"));

    assert_eq!(run(&s, &format!("{q} ORDER BY age DESC MODE materialized TOPK 2")), "+OK\r\n");
    let top = run(&s, "VIEW.QUERY v");
    assert!(top.find("u:5").unwrap() < top.find("u:4").unwrap(), "{top}");
    assert!(run(&s, &format!("{q} ORDER BY age")).starts_with("-ERR"), "a second view named v");
}

#[test]
fn idx_advise_takes_no_arguments() {
    let s = store();
    assert_eq!(run(&s, "IDX.ADVISE now"), err("ERR usage: IDX.ADVISE"));
}
