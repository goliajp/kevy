//! The `TABLE.DECLARE` wire grammar — clause-scan over argv, shared by
//! the server and the embedded dispatch (ONE parser, so the two wire
//! faces cannot drift; the dispatch oracle byte-compares them anyway).

use crate::catalog::{IndexKind, ValType};
use crate::table::{OrderPath, TableIndex, TableSpec, WindowSpec, dotted};

/// The usage line every malformed `TABLE.DECLARE` answers with.
pub const TABLE_DECLARE_USAGE: &str = "ERR usage: TABLE.DECLARE name PREFIX p PK col COLUMN name i64|f64|str [COLUMN ...] [INDEX col range|unique [VALUES col ...] [GLOBAL [SPLIT AT v ...]]] [ORDERPATH name ON col [DESC] [THEN col [DESC]] ... [GLOBAL]] [WINDOW col SPAN n BUCKET n] [AUTODECLARE n]";

/// A table path declared `GLOBAL`: spread over the shards by value, as
/// `IDX.CREATE … PARTITION global` spreads an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalPath {
    /// The compiled index's name, `<table>.<column>` or `<table>.<orderpath>`.
    pub path: Vec<u8>,
    /// The `SPLIT AT` values as written; empty = sampled from the rows.
    pub split_at: Vec<Vec<u8>>,
}

/// A table-declaration clause keyword — the boundary variadic lists
/// (`VALUES`, the `ORDERPATH` column chain) collect up to.
fn is_table_kw(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"COLUMN")
        || a.eq_ignore_ascii_case(b"INDEX")
        || a.eq_ignore_ascii_case(b"ORDERPATH")
        || a.eq_ignore_ascii_case(b"WINDOW")
        || a.eq_ignore_ascii_case(b"AUTODECLARE")
        || a.eq_ignore_ascii_case(b"GLOBAL")
}

/// Parse a full `TABLE.DECLARE` argv into a validated [`TableSpec`].
/// `Err` carries the exact wire error — usage on structural misses,
/// a named refusal for everything semantic (unknown column, bad type,
/// duplicates …). A `GLOBAL` path is refused by name: this is the parse
/// for a store whose paths are all local (see
/// [`parse_table_declare_partitioned`]).
pub fn parse_table_declare(argv: &[&[u8]]) -> Result<TableSpec, String> {
    let (spec, globals) = parse_table_declare_partitioned(argv)?;
    if !globals.is_empty() {
        return Err("ERR GLOBAL is a server feature; an embedded store's paths are local".into());
    }
    Ok(spec)
}

/// [`parse_table_declare`] that also returns the paths declared `GLOBAL`.
/// A windowed table's paths are refused `GLOBAL`: eviction moves a row's
/// entries out on the row's own shard, where a global path's entries are
/// not.
///
/// ```
/// use kevy_index::parse_table_declare_partitioned;
///
/// let (t, global) = parse_table_declare_partitioned(&[
///     b"TABLE.DECLARE", b"u", b"PREFIX", b"u:", b"PK", b"id", b"COLUMN", b"id", b"i64",
///     b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range", b"GLOBAL", b"SPLIT", b"AT", b"40",
/// ])
/// .unwrap();
/// assert_eq!(t.indexes.len(), 1);
/// assert_eq!(global[0].path, b"u.age");
/// assert_eq!(global[0].split_at, [b"40".to_vec()]);
/// ```
pub fn parse_table_declare_partitioned(
    argv: &[&[u8]],
) -> Result<(TableSpec, Vec<GlobalPath>), String> {
    let mut spec = table_head(argv)?;
    let mut i = 6;
    let mut globals = Vec::new();
    while i < argv.len() {
        let kw = argv[i];
        if kw.eq_ignore_ascii_case(b"COLUMN") {
            i = parse_column(argv, i + 1, &mut spec)?;
        } else if kw.eq_ignore_ascii_case(b"INDEX") {
            i = parse_index(argv, i + 1, &mut spec)?;
            i = parse_global(
                argv,
                i,
                &spec.name,
                &spec.indexes.last().expect("pushed").column,
                &mut globals,
            )?;
        } else if kw.eq_ignore_ascii_case(b"ORDERPATH") {
            i = parse_orderpath(argv, i + 1, &mut spec)?;
            i = parse_global(
                argv,
                i,
                &spec.name,
                &spec.orderpaths.last().expect("pushed").name,
                &mut globals,
            )?;
        } else if kw.eq_ignore_ascii_case(b"WINDOW") {
            i = parse_window(argv, i + 1, &mut spec)?;
        } else if kw.eq_ignore_ascii_case(b"AUTODECLARE") {
            i = parse_autodeclare(argv, i + 1, &mut spec)?;
        } else {
            return Err(TABLE_DECLARE_USAGE.into());
        }
    }
    spec.validate()?;
    if spec.window.is_some() && !globals.is_empty() {
        return Err("ERR GLOBAL cannot apply to a windowed table's paths: eviction moves a row's entries out on the row's own shard".into());
    }
    Ok((spec, globals))
}

/// `TABLE.DECLARE name PREFIX p PK col` and the first `COLUMN` keyword: the
/// spec before its clauses.
fn table_head(argv: &[&[u8]]) -> Result<TableSpec, String> {
    if argv.len() < 9
        || !argv[2].eq_ignore_ascii_case(b"PREFIX")
        || !argv[4].eq_ignore_ascii_case(b"PK")
        || !argv[6].eq_ignore_ascii_case(b"COLUMN")
    {
        return Err(TABLE_DECLARE_USAGE.into());
    }
    Ok(TableSpec {
        name: argv[1].to_vec(),
        prefix: argv[3].to_vec(),
        pk: argv[5].to_vec(),
        columns: Vec::new(),
        indexes: Vec::new(),
        orderpaths: Vec::new(),
        window: None,
        autodeclare: 0,
        auto_added: Vec::new(),
    })
}

/// `[GLOBAL [SPLIT AT <v>…]]` after the path `suffix` of table `table`.
fn parse_global(
    argv: &[&[u8]],
    at: usize,
    table: &[u8],
    suffix: &[u8],
    globals: &mut Vec<GlobalPath>,
) -> Result<usize, String> {
    if !argv.get(at).is_some_and(|a| a.eq_ignore_ascii_case(b"GLOBAL")) {
        return Ok(at);
    }
    let mut i = at + 1;
    let mut split_at = Vec::new();
    if argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(b"SPLIT")) {
        if !argv.get(i + 1).is_some_and(|a| a.eq_ignore_ascii_case(b"AT")) {
            return Err(TABLE_DECLARE_USAGE.into());
        }
        i += 2;
        while i < argv.len() && !is_table_kw(argv[i]) {
            split_at.push(argv[i].to_vec());
            i += 1;
        }
        if split_at.is_empty() {
            return Err("ERR SPLIT AT needs at least one value".into());
        }
    }
    globals.push(GlobalPath { path: dotted(table, suffix), split_at });
    Ok(i)
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

/// `AUTODECLARE <n>` — the engine may declare at most `n` paths for
/// this table from observed refusals.
fn parse_autodeclare(argv: &[&[u8]], at: usize, spec: &mut TableSpec) -> Result<usize, String> {
    if spec.autodeclare != 0 {
        return Err("ERR duplicate AUTODECLARE clause".into());
    }
    let n: usize = argv
        .get(at)
        .and_then(|raw| str::from_utf8(raw).ok())
        .and_then(|t| t.parse().ok())
        .ok_or("ERR AUTODECLARE needs a positive integer")?;
    if n == 0 {
        return Err("ERR AUTODECLARE needs a positive integer".into());
    }
    spec.autodeclare = n;
    Ok(at + 1)
}

/// `WINDOW <col> SPAN <n> BUCKET <n>` — plain integers in the window
/// column's own units; the engine never assumes a time base.
fn parse_window(argv: &[&[u8]], at: usize, spec: &mut TableSpec) -> Result<usize, String> {
    if spec.window.is_some() {
        return Err("ERR duplicate WINDOW clause".into());
    }
    let (Some(col), Some(span_kw), Some(span_raw), Some(bucket_kw), Some(bucket_raw)) =
        (argv.get(at), argv.get(at + 1), argv.get(at + 2), argv.get(at + 3), argv.get(at + 4))
    else {
        return Err(TABLE_DECLARE_USAGE.into());
    };
    if !span_kw.eq_ignore_ascii_case(b"SPAN") || !bucket_kw.eq_ignore_ascii_case(b"BUCKET") {
        return Err(TABLE_DECLARE_USAGE.into());
    }
    let int = |raw: &[u8], what: &str| -> Result<i64, String> {
        str::from_utf8(raw)
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| format!("ERR WINDOW {what} must be an integer"))
    };
    spec.window = Some(WindowSpec {
        column: col.to_vec(),
        span: int(span_raw, "SPAN")?,
        bucket: int(bucket_raw, "BUCKET")?,
    });
    Ok(at + 5)
}
