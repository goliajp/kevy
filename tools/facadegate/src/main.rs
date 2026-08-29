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
    // The families as data, so the pass line counts what actually ran
    // rather than a number someone remembered to update. This release
    // argued that "0 hits" and "0 hits across 709 files" are different
    // sentences; a gate of its own that printed only PASS was the same
    // sentence missing its second half.
    let families: [(&str, fn()); 4] = [
        ("kv+hash", kv_and_hash),
        ("tables", tables_end_to_end),
        ("bad specs refuse", bad_specs_are_refusals_not_panics),
        ("ensure boots", ensure_is_the_boot_verb),
    ];
    for (_, f) in families {
        f();
    }
    io_result_world_uses_question_mark().expect("io interop");
    let _ = client_surface_compiles;
    println!(
        "facadegate: PASS — {} families end-to-end plus io interop, and the \
         network client's public surface, through facade imports only",
        families.len(),
    );
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
        window: None,
        autodeclare: 0,
        auto_added: vec![],
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

    // Plant one row per exclusion cause (v4.1-V4): the fresh
    // two-directional VERIFY names each of them instead of leaving an
    // unexplained entries-vs-rows diff.
    store
        .hset(b"row:absent", &[(&b"user"[..], &b"u9"[..])])
        .expect("row missing `activity` — NULL by design");
    store
        .hset(b"row:coerce", &[(&b"user"[..], &b"u9"[..]), (&b"activity"[..], &b"NaN"[..])])
        .expect("row whose `activity` fails i64 coercion");
    let long = vec![b'x'; 300]; // over MAX_STR_COMPONENT (255)
    store
        .hset(b"row:oversize", &[(&b"user"[..], &long[..]), (&b"activity"[..], &b"1"[..])])
        .expect("row whose composite str component is oversize");

    // The named verify report — every counter fresh, both directions,
    // replacing the 4.0 tuple of anonymous arrays (whose
    // coerce_failures was a lifetime tally that also counted absences).
    let report = store.table_verify_report(b"threads").expect("verify");
    assert_eq!(report.per_index.len(), 2, "one index + one orderpath");
    for ix in &report.per_index {
        assert_eq!(ix.drift, 0, "fresh rows cannot drift: {:?}", ix.name);
        assert_eq!(ix.rows, 53, "every prefix row is walked: {:?}", ix.name);
        assert_eq!(ix.missing, 0, "no forgotten writer here: {:?}", ix.name);
    }
    let by_user = &report.per_index[0]; // single column `user`
    assert_eq!(by_user.entries, 53, "all 53 rows carry `user` — a str single coerces anything");
    let composite = &report.per_index[1]; // by_user_activity
    assert_eq!(composite.entries, 50, "the three planted rows are all excluded from the composite");
    assert_eq!(composite.absent, 1, "row:absent — missing component is NULL, not an error");
    assert_eq!(composite.coerce_failures, 1, "row:coerce — present but not an i64");
    assert_eq!(composite.excluded, 1, "row:oversize — the 255-byte cap, named at last");
    assert_eq!(
        report.spot_type_mismatches, 1,
        "the spot check flags row:coerce too — same fact, second witness"
    );

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
        window: None,
        autodeclare: 0,
        auto_added: vec![],
    };
    let err = store.table_declare(bad).expect_err("undeclared ORDERPATH column must refuse");
    let msg = format!("{err}");
    assert!(msg.contains("unknown column") || msg.contains("not declared"), "unhelpful refusal: {msg}");
    assert!(store.table_list().is_empty(), "a refused declare must install nothing");
}

/// The boot pattern (dogfood F8.2): declaring at boot is the steady
/// state, and `ensure` is its verb — identical spec is a no-op success,
/// a changed spec is a named refusal (never a silent rebuild), and
/// `replace` is the explicit rebuild.
fn ensure_is_the_boot_verb() {
    use kevy_embedded::TableEnsure;
    let store = Store::open(Config::default()).expect("mem store opens");
    let spec = || TableSpec {
        name: b"t".to_vec(),
        prefix: b"row:".to_vec(),
        pk: b"user".to_vec(),
        columns: vec![(b"user".to_vec(), IndexValType::Str)],
        indexes: vec![TableIndex {
            column: b"user".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: None,
        autodeclare: 0,
        auto_added: vec![],
    };
    assert_eq!(store.table_ensure(spec()).expect("first boot"), TableEnsure::Created);
    assert_eq!(store.table_ensure(spec()).expect("every later boot"), TableEnsure::Unchanged);

    let mut changed = spec();
    changed.columns.push((b"extra".to_vec(), IndexValType::I64));
    let err = store.table_ensure(changed.clone()).expect_err("a changed spec must refuse");
    assert!(format!("{err}").contains("COLUMNS"), "the refusal names what changed: {err}");

    store.table_replace(changed).expect("the explicit rebuild verb");
    assert_eq!(store.table_list()[0].columns.len(), 2, "replace installed the new shape");
}

/// The io::Result interop, from consumer position (v4.1-V7, dogfood
/// F2): a function stuck in an `io::Result` world uses `?` on kevy
/// calls directly — the conversion kevy must provide (orphan rule),
/// and which one consumer hand-wrote ~280 times as `io::Error::other`
/// on 4.0. Kind-mapped and source-preserving: the typed error is
/// still there behind the boundary.
fn io_result_world_uses_question_mark() -> std::io::Result<()> {
    let store = Store::open(Config::default().with_auto_aof_rewrite_disabled())?;
    store.set(b"k", b"v")?;
    assert_eq!(
        store.downgradeable_to_v3(),
        None,
        "memory-only: the downgrade question has no meaning"
    );
    let err: std::io::Error =
        kevy_embedded::KevyError::TimedOut.into();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "kind survives the boundary");
    assert!(
        err.get_ref().is_some_and(|s| s.is::<kevy_embedded::KevyError>()),
        "the typed error rides as source, downcastable back out"
    );
    Ok(())
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

/// The async twin, including the io::Result interop both client
/// crates inherit from the shared error type (v4.1-V7). Never called
/// — running needs a server; compiling needs the facade to be whole.
#[allow(dead_code)]
async fn async_client_surface_compiles() -> std::io::Result<()> {
    let mut conn = kevy_client_async::AsyncConnection::connect("127.0.0.1:6399").await?;
    conn.set(b"k", b"v").await?;
    let _v: Option<Vec<u8>> = conn.get(b"k").await?;
    Ok(())
}
