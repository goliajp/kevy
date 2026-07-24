//! The `TABLE.DECLARE` wire grammar — clause-scan over argv, shared by
//! the server and the embedded dispatch (ONE parser, so the two wire
//! faces cannot drift; the dispatch oracle byte-compares them anyway).

use crate::catalog::{IndexKind, ValType};
use crate::table::{OrderPath, TableIndex, TableSpec};

/// The usage line every malformed `TABLE.DECLARE` answers with.
pub const TABLE_DECLARE_USAGE: &str = "ERR usage: TABLE.DECLARE name PREFIX p PK col COLUMN name i64|f64|str [COLUMN ...] [INDEX col range|unique [VALUES col ...]] [ORDERPATH name ON col [DESC] [THEN col [DESC]] ...]";

/// A table-declaration clause keyword — the boundary variadic lists
/// (`VALUES`, the `ORDERPATH` column chain) collect up to.
fn is_table_kw(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"COLUMN")
        || a.eq_ignore_ascii_case(b"INDEX")
        || a.eq_ignore_ascii_case(b"ORDERPATH")
}

/// Parse a full `TABLE.DECLARE` argv into a validated [`TableSpec`].
/// `Err` carries the exact wire error — usage on structural misses,
/// a named refusal for everything semantic (unknown column, bad type,
/// duplicates …).
pub fn parse_table_declare(argv: &[&[u8]]) -> Result<TableSpec, String> {
    if argv.len() < 9
        || !argv[2].eq_ignore_ascii_case(b"PREFIX")
        || !argv[4].eq_ignore_ascii_case(b"PK")
        || !argv[6].eq_ignore_ascii_case(b"COLUMN")
    {
        return Err(TABLE_DECLARE_USAGE.into());
    }
    let mut spec = TableSpec {
        name: argv[1].to_vec(),
        prefix: argv[3].to_vec(),
        pk: argv[5].to_vec(),
        columns: Vec::new(),
        indexes: Vec::new(),
        orderpaths: Vec::new(),
    };
    let mut i = 6;
    while i < argv.len() {
        let kw = argv[i];
        if kw.eq_ignore_ascii_case(b"COLUMN") {
            i = parse_column(argv, i + 1, &mut spec)?;
        } else if kw.eq_ignore_ascii_case(b"INDEX") {
            i = parse_index(argv, i + 1, &mut spec)?;
        } else if kw.eq_ignore_ascii_case(b"ORDERPATH") {
            i = parse_orderpath(argv, i + 1, &mut spec)?;
        } else {
            return Err(TABLE_DECLARE_USAGE.into());
        }
    }
    spec.validate()?;
    Ok(spec)
}

/// `COLUMN <name> <i64|f64|str>` — returns the next clause index.
fn parse_column(argv: &[&[u8]], at: usize, spec: &mut TableSpec) -> Result<usize, String> {
    let (Some(name), Some(ty_raw)) = (argv.get(at), argv.get(at + 1)) else {
        return Err(TABLE_DECLARE_USAGE.into());
    };
    let ty = match ValType::parse(ty_raw) {
        Some(t @ (ValType::I64 | ValType::F64 | ValType::Str)) => t,
        _ => return Err("ERR COLUMN type must be i64|f64|str".into()),
    };
    spec.columns.push((name.to_vec(), ty));
    Ok(at + 2)
}

/// `INDEX <col> <range|unique> [VALUES <col>…]`.
fn parse_index(argv: &[&[u8]], at: usize, spec: &mut TableSpec) -> Result<usize, String> {
    let (Some(col), Some(kind_raw)) = (argv.get(at), argv.get(at + 1)) else {
        return Err(TABLE_DECLARE_USAGE.into());
    };
    let kind = match IndexKind::parse(kind_raw) {
        Some(k @ (IndexKind::Range | IndexKind::Unique)) => k,
        _ => return Err("ERR INDEX kind must be range|unique".into()),
    };
    let mut values = Vec::new();
    let mut i = at + 2;
    if argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(b"VALUES")) {
        i += 1;
        while i < argv.len() && !is_table_kw(argv[i]) {
            values.push(argv[i].to_vec());
            i += 1;
        }
        if values.is_empty() {
            return Err("ERR VALUES needs at least one column".into());
        }
    }
    spec.indexes.push(TableIndex { column: col.to_vec(), kind, values });
    Ok(i)
}

/// `ORDERPATH <name> ON <col> [DESC] [THEN <col> [DESC]]…`.
fn parse_orderpath(argv: &[&[u8]], at: usize, spec: &mut TableSpec) -> Result<usize, String> {
    let (Some(name), Some(on_kw)) = (argv.get(at), argv.get(at + 1)) else {
        return Err(TABLE_DECLARE_USAGE.into());
    };
    if !on_kw.eq_ignore_ascii_case(b"ON") {
        return Err("ERR ORDERPATH needs ON <col>".into());
    }
    let mut on = Vec::new();
    let mut i = at + 2;
    loop {
        let Some(col) = argv.get(i) else {
            return Err("ERR ORDERPATH needs ON <col>".into());
        };
        if is_table_kw(col) {
            return Err("ERR ORDERPATH needs ON <col>".into());
        }
        let mut desc = false;
        i += 1;
        if argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(b"DESC")) {
            desc = true;
            i += 1;
        }
        on.push((col.to_vec(), desc));
        match argv.get(i) {
            Some(a) if a.eq_ignore_ascii_case(b"THEN") => i += 1,
            Some(a) if is_table_kw(a) => break,
            None => break,
            Some(_) => return Err(TABLE_DECLARE_USAGE.into()),
        }
    }
    spec.orderpaths.push(OrderPath { name: name.to_vec(), on });
    Ok(i)
}
