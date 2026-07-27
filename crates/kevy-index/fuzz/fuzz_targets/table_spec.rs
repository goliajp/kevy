#![no_main]
//! The declaration path must never panic, whatever the spec says.
//!
//! Dogfood F9: a TableSpec whose ORDERPATH named an undeclared column
//! panicked inside `compile_table` on a consumer's boot path and
//! restart-looped their production container. The fix made
//! `compile_table` validate for itself and return `Err`; this target is
//! the standing proof that no spec — parsed from arbitrary wire bytes
//! or assembled from arbitrary parts — can reach a panic again.
//!
//! Two routes, both driven from the same input:
//! 1. the wire route: bytes → argv split → `parse_table_declare` →
//!    (validate) → compile;
//! 2. the typed route: a TableSpec assembled directly from the raw
//!    parts, `compile_table` called on it cold — the embedded-face
//!    shape, which is the one that shipped panicking.

use libfuzzer_sys::fuzz_target;

use kevy_index::{IndexKind, OrderPath, TableIndex, TableSpec, ValType, compile_table};

fuzz_target!(|data: &[u8]| {
    // Route 1: wire bytes, split on 0xFF into argv-ish chunks.
    let argv: Vec<&[u8]> = data.split(|b| *b == 0xFF).collect();
    if let Ok(spec) = kevy_index::parse_table_declare(&argv) {
        // A parsed spec compiles or refuses; either way, no panic.
        let _ = compile_table(&spec);
    }

    // Route 2: a typed spec assembled from raw fragments, unvalidated —
    // exactly what an embedded caller can hand to table_declare.
    let mut parts = data.split(|b| *b == 0xFE).map(<[u8]>::to_vec);
    let mut next = || parts.next().unwrap_or_default();
    let ty_of = |b: &[u8]| match b.first().copied().unwrap_or(0) % 3 {
        0 => ValType::Str,
        1 => ValType::I64,
        _ => ValType::F64,
    };
    let col_a = next();
    let col_b = next();
    let spec = TableSpec {
        name: next(),
        prefix: next(),
        pk: next(),
        columns: vec![(col_a.clone(), ty_of(&col_a)), (col_b.clone(), ty_of(&col_b))],
        indexes: vec![TableIndex {
            column: next(),
            kind: if data.len() % 2 == 0 { IndexKind::Range } else { IndexKind::Unique },
            values: vec![next()],
        }],
        orderpaths: vec![OrderPath {
            name: next(),
            on: vec![(next(), true), (next(), false)],
        }],
    };
    let _ = compile_table(&spec);
});
