//! The embedded face of the auto-declaration loop: typed-API
//! refusals land in the advise log, [`Store::idx_advise`] renders
//! each family as the declaration that would serve it, and any
//! catalog mutation clears the slate — mirroring the wire face's
//! IDX.ADVISE e2e.

#![cfg(feature = "index")]

use kevy_embedded::{Config, ScalarQueryOpts, Store, ValueFilter};
use kevy_index::{IndexKind, IndexValue, TableIndex, TableSpec, ValType};

/// Columns only, plus one index WITHOUT stored values — each refusal
/// below is one missing declaration away from serving.
fn lean_table() -> TableSpec {
    TableSpec {
        name: b"ev".to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![
            (b"id".to_vec(), ValType::Str),
            (b"at".to_vec(), ValType::I64),
            (b"age".to_vec(), ValType::I64),
            (b"note".to_vec(), ValType::Str),
        ],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: None,
    }
}

#[test]
fn refusals_render_and_catalog_mutations_clear() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).expect("open");
    s.table_declare(lean_table()).expect("declare");

    // A fresh path is "never hit" — the reclaim face lists it with
    // its age until the first served query retires the suggestion.
    let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(100));
    let fresh = s.idx_advise();
    assert_eq!(fresh.len(), 1, "{fresh:?}");
    assert!(fresh[0].advice.starts_with("IDX.DROP ev.at"), "{}", fresh[0].advice);
    s.idx_count(b"ev.at", &lo, &hi).expect("served");
    assert!(s.idx_advise().is_empty(), "a served path needs no advice");
    let (hits, _, _) = s.idx_usage(b"ev.at").expect("declared");
    assert_eq!(hits, 1, "the served query counted");

    // A Range family, twice — it ranks first.
    assert!(s.idx_count(b"ev.age", &lo, &hi).is_err());
    assert!(s.idx_query(b"ev.age", &lo, &hi, None, 10).is_err());

    // A Filter family: the declared path exists, the field is not
    // stored.
    let filters = [ValueFilter::Eq { field: b"note", value: b"x" }];
    let opts = ScalarQueryOpts { filters: &filters, ..ScalarQueryOpts::default() };
    assert!(s.idx_query_claused(b"ev.at", &lo, &hi, None, 10, opts).is_err());

    // A Match family on an undeclared text path.
    #[cfg(feature = "text")]
    assert!(s.idx_match(b"ev.note", b"hello", 10).is_err());

    let advice = s.idx_advise();
    let expect = 2 + usize::from(cfg!(feature = "text"));
    assert_eq!(advice.len(), expect, "{advice:?}");
    assert_eq!(advice[0].name, b"ev.age", "most-refused family first");
    assert_eq!(advice[0].count, 2);
    assert!(advice[0].advice.contains("INDEX age range"), "{}", advice[0].advice);
    assert!(
        advice.iter().any(|a| a.advice == "add VALUES note (type str) to the ev.at declaration"),
        "{advice:?}"
    );
    #[cfg(feature = "text")]
    assert!(
        advice
            .iter()
            .any(|a| a.advice == "IDX.CREATE ev.note ON PREFIX ev: FIELD note TYPE str KIND text"),
        "{advice:?}"
    );

    // An ungrounded family (unknown column) is observed but withheld.
    assert!(s.idx_count(b"ev.ghost", &lo, &hi).is_err());
    assert_eq!(s.idx_advise().len(), expect, "ungrounded family withheld");

    // Any catalog mutation clears the slate.
    assert!(s.idx_drop(b"ev.at"));
    assert!(s.idx_advise().is_empty(), "mutation clears the log");
}
