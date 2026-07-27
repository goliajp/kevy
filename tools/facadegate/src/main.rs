//! The consumer-position gate.
//!
//! This binary stands where an outside consumer stands: its own
//! workspace, its own lockfile, and **only facade imports** — every
//! `use` below is `kevy_embedded::…` or `kevy_client::…`, never an
//! internal crate. If a public method's parameter or return type is
//! missing from the facade, this crate stops compiling, which is the
//! failure mode F7 deserved and did not get: `Store::table_declare`
//! shipped in 4.0 taking a `TableSpec` the facade never exported, so
//! the typed face of a flagship feature was uncallable — and every
//! in-workspace test resolved the type anyway and saw nothing wrong.
//!
//! Each family below is exercised end-to-end where that is cheap, and
//! at the type level where it needs external infrastructure (the
//! network client compiles against its full public surface; running it
//! needs a server this crate deliberately does not have).

use kevy_embedded::{
    Config, IndexKind, IndexValue, OrderPath, IndexValType, Store, TableIndex, TableSpec,
};

fn main() {
    kv_and_hash();
    tables_end_to_end();
    bad_specs_are_refusals_not_panics();
    let _ = client_surface_compiles;
    println!("facadegate: PASS");
}

/// KV + hash through the facade — the rows tables are declared over.
fn kv_and_hash() {
    let store = Store::open(Config::default()).expect("mem store opens");
    store.set(b"k", b"v").expect("set");
    assert_eq!(store.get(b"k").expect("get").as_deref(), Some(&b"v"[..]));
    store
        .hset(b"row:1", &[(&b"user"[..], &b"u1"[..]), (&b"activity"[..], &b"100"[..])])
        .expect("hset");
    let _ = store.del(&[&b"k"[..]]);
}

/// The full TABLE chain: declare → write → verify → list → drop, all
/// through facade types. This is the path 4.0 shipped uncallable.
fn tables_end_to_end() {
    let store = Store::open(Config::default()).expect("mem store opens");
    let spec = TableSpec {
        name: b"threads".to_vec(),
        prefix: b"row:".to_vec(),
        pk: b"user".to_vec(),
        columns: vec![
            (b"user".to_vec(), IndexValType::Str),
            (b"activity".to_vec(), IndexValType::I64),
        ],
        indexes: vec![TableIndex {
            column: b"user".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![OrderPath {
            name: b"by_user_activity".to_vec(),
            on: vec![(b"user".to_vec(), false), (b"activity".to_vec(), true)],
        }],
    };
    store.table_declare(spec).expect("declare through the facade");

    for i in 0..50u32 {
        let key = format!("row:{i}");
        let user = format!("u{}", i % 5);
        let act = format!("{}", 1000 + i);
        store
            .hset(key.as_bytes(), &[(&b"user"[..], user.as_bytes()), (&b"activity"[..], act.as_bytes())])
            .expect("row write");
    }

    // The compiled single-column index answers a typed range query.
    // (Another anonymous tuple in the public surface, noted for the
    // same treatment VERIFY got — the gate documents what it finds.)
    let (hits, _cursor) = store
        .idx_query(
            b"threads.user",
            &IndexValue::Str(b"u1".to_vec()),
            &IndexValue::Str(b"u1".to_vec()),
            None,
            100,
        )
        .expect("typed query on the compiled index");
    assert_eq!(hits.len(), 10, "five users, fifty rows");

    // The named verify report — counters with names and stated
    // time semantics, replacing the 4.0 tuple of anonymous arrays.
    let report = store.table_verify_report(b"threads").expect("verify");
    assert_eq!(report.per_index.len(), 2, "one index + one orderpath");
    for ix in &report.per_index {
        assert_eq!(ix.drift, 0, "fresh rows cannot drift: {:?}", ix.name);
        assert_eq!(ix.entries, 50, "{:?}", ix.name);
    }
    assert_eq!(report.spot_type_mismatches, 0);

    assert_eq!(store.table_list().len(), 1);
    assert!(store.table_drop(b"threads"));
}

/// The exact spec that took a consumer's production down (dogfood F9):
/// an ORDERPATH naming a column that was never declared. In 4.0 the
/// typed path skipped validation and this panicked inside
/// `compile_table` — on the consumer's boot path, which restart-looped
/// their container. The guarantee now: a bad spec is a named `Err` from
/// `table_declare`, whatever is wrong with it.
fn bad_specs_are_refusals_not_panics() {
    let store = Store::open(Config::default()).expect("mem store opens");
    let bad = TableSpec {
        name: b"t".to_vec(),
        prefix: b"row:".to_vec(),
        pk: b"user".to_vec(),
        columns: vec![(b"user".to_vec(), IndexValType::Str)],
        indexes: vec![],
        orderpaths: vec![OrderPath {
            name: b"by_ord".to_vec(),
            // `ord` is not in `columns` — the F9 spec, byte for byte.
            on: vec![(b"user".to_vec(), false), (b"ord".to_vec(), true)],
        }],
    };
    let err = store.table_declare(bad).expect_err("undeclared ORDERPATH column must refuse");
    let msg = format!("{err}");
    assert!(msg.contains("unknown column") || msg.contains("not declared"), "unhelpful refusal: {msg}");
    assert!(store.table_list().is_empty(), "a refused declare must install nothing");
}

/// The network client's public surface, compile-checked from consumer
/// position. Never called — running needs a server — but every type a
/// caller would name has to resolve through the facade for this to
/// build.
#[allow(dead_code)]
fn client_surface_compiles() -> kevy_client::KevyResult<()> {
    let mut conn = kevy_client::Connection::connect("127.0.0.1:6399")?;
    conn.set(b"k", b"v")?;
    let _v: Option<Vec<u8>> = conn.get(b"k")?;
    Ok(())
}
