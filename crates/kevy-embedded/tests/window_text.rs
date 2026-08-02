//! The embedded face's text-window gate — the server e2e's mirror: a
//! windowed table's TEXT index freezes its out-of-window documents
//! into cold bucket segments through the reaper's tick, and MATCH
//! answers — scores, sort keys, facet counts and highlight spans
//! included — must equal a never-windowed control's over the same
//! rows, for every clause with a cold path, through churn. The
//! dictionary-shaped clauses (prefix, TYPO, IN) refuse by name.

#![cfg(all(feature = "index", feature = "text", feature = "persist", not(target_arch = "wasm32")))]

use std::time::{Duration, Instant};

use kevy_embedded::{Config, MatchOpts, Store, ValueFilter};
use kevy_index::{IndexKind, TableIndex, TableSpec, ValType, WindowSpec};

fn run(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    assert!(!out.starts_with(b"-ERR"), "{}", String::from_utf8_lossy(&out));
    out
}

fn table(name: &[u8], windowed: bool) -> TableSpec {
    TableSpec {
        name: name.to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![
            (b"id".to_vec(), ValType::Str),
            (b"at".to_vec(), ValType::I64),
            (b"prio".to_vec(), ValType::I64),
            (b"tag".to_vec(), ValType::Str),
        ],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: windowed.then_some(WindowSpec {
            column: b"at".to_vec(),
            span: 100,
            bucket: 10,
        }),
    }
}

#[test]
fn embedded_text_window_freezes_and_stays_semantically_equivalent() {
    let d = kevy_tmpdir::TmpDir::new("emb-wintext");
    let s = Store::open(
        Config::default()
            .with_persist(d.path())
            .with_reaper_interval(Duration::from_millis(25)),
    )
    .expect("open");

    s.table_declare(table(b"ev", true)).expect("declare ev");
    s.table_declare(table(b"ctl", false)).expect("declare ctl");
    for name in [b"ev.note".as_slice(), b"ctl.note"] {
        s.idx_create_text(
            name,
            b"ev:",
            &[(b"note", 1.0)],
            true,
            &[(b"prio", ValType::I64), (b"tag", ValType::Str)],
        )
        .expect("create text index");
    }

    let vocab = ["rust engine warm", "storage engine warm", "python glue warm",
                 "rust storage cold path", "engine of record warm"];
    let tags = ["alpha", "beta", "gamma"];
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        let prio = ((i % 4) * 10).to_string();
        let note = vocab[(i % 5) as usize];
        let mut argv: Vec<&[u8]> = vec![b"HSET", key.as_bytes(), b"id", key.as_bytes(),
            b"at", at.as_bytes(), b"note", note.as_bytes(), b"prio", prio.as_bytes()];
        let tag = tags[(i % 3) as usize];
        if i % 7 != 0 {
            argv.extend_from_slice(&[b"tag", tag.as_bytes()]);
        }
        run(&s, &argv);
    }

    // Wait for the reaper's freeze: a txt segment appears.
    let segs = d.path().join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frozen = segs.exists()
            && std::fs::read_dir(&segs).is_ok_and(|r| {
                r.filter_map(Result::ok)
                    .any(|e| e.file_name().to_string_lossy().starts_with("txt-"))
            });
        if frozen {
            break;
        }
        assert!(Instant::now() < deadline, "embedded text never froze");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Every clause with a cold path, page and facets equal to the
    // control: terms, phrases, FILTER, SORT, DISTINCT, FACET,
    // HIGHLIGHT, combinations.
    let filter_prio = [ValueFilter::Range { field: b"prio", min: b"10", max: b"20" }];
    let facet_tag = [b"tag".to_vec()];
    let hl_all: [Vec<u8>; 0] = [];
    let compare = |tag_label: &str| {
        let shapes: &[(&str, &[u8], MatchOpts)] = &[
            ("term", b"rust", MatchOpts::default()),
            ("or", b"warm engine", MatchOpts::default()),
            ("absent", b"absent", MatchOpts::default()),
            ("phrase", b"\"rust engine\"", MatchOpts::default()),
            ("mixed", b"warm \"rust storage\"", MatchOpts::default()),
            ("filter", b"rust", MatchOpts { filters: &filter_prio, ..Default::default() }),
            ("sort", b"engine", MatchOpts { sort: Some((b"prio", true)), ..Default::default() }),
            ("distinct", b"engine", MatchOpts { distinct: Some(b"tag"), ..Default::default() }),
            ("facet", b"rust", MatchOpts { facets: &facet_tag, ..Default::default() }),
            ("highlight", b"\"rust engine\"", MatchOpts { highlight: Some(&hl_all), ..Default::default() }),
            (
                "combo",
                b"rust",
                MatchOpts {
                    filters: &filter_prio,
                    sort: Some((b"prio", false)),
                    offset: 1,
                    ..Default::default()
                },
            ),
        ];
        for (label, query, opts) in shapes {
            let ev = s.idx_match_faceted(b"ev.note", query, 30, *opts).expect("ev page");
            let ctl = s.idx_match_faceted(b"ctl.note", query, 30, *opts).expect("ctl page");
            assert_eq!(ev.hits, ctl.hits, "{tag_label}: {label} hits");
            assert_eq!(ev.facets, ctl.facets, "{tag_label}: {label} facets");
        }
    };
    compare("after freeze");

    // Churn: rewrite a cold row's note AND values (revives hot +
    // withdraws the frozen statistics exactly), delete another.
    run(&s, &[b"HSET", b"ev:3", b"id", b"ev:3", b"at", b"30",
        b"note", b"rust replaced text", b"prio", b"5", b"tag", b"beta"]);
    run(&s, &[b"DEL", b"ev:7"]);
    std::thread::sleep(Duration::from_millis(200));
    compare("after churn");

    // The dictionary-shaped clauses refuse on the cold index by name,
    // and serve on the control.
    let err = s
        .idx_match_faceted(b"ev.note", b"rus*", 5, MatchOpts::default())
        .expect_err("prefix must refuse on cold");
    assert!(format!("{err}").contains("not built yet"), "{err}");
    let err = s
        .idx_match_faceted(b"ev.note", b"rusk", 5, MatchOpts { typo: 1, ..Default::default() })
        .expect_err("TYPO must refuse on cold");
    assert!(format!("{err}").contains("not built yet"), "{err}");
    let scope = [b"note".to_vec()];
    let err = s
        .idx_match_faceted(b"ev.note", b"rust", 5, MatchOpts { scope: &scope, ..Default::default() })
        .expect_err("IN must refuse on cold");
    assert!(format!("{err}").contains("not built yet"), "{err}");
    for (query, opts) in [
        (b"rus*".as_slice(), MatchOpts::default()),
        (b"rusk", MatchOpts { typo: 1, ..Default::default() }),
        (b"rust", MatchOpts { scope: &scope, ..Default::default() }),
    ] {
        s.idx_match_faceted(b"ctl.note", query, 5, opts).expect("control serves");
    }
}
