//! The two faces that start from a live catalog instead of a schema
//! file: [`select_card`] (`kevy-cli sql run`) and [`table_ddl`]
//! (`kevy-cli show-create --as sql`). Both take `TABLE.DECLARE` argv —
//! what `TABLE.DESCRIBE` returns as `declaration` — so the compiler
//! plans against exactly what the server holds.

use crate::ast::CreateView;
use crate::declared::{self, Declared};
use crate::lex::{Tok, lex};
use crate::parse::P;
use crate::schema::Table;
use crate::{KevyType, QueryCard, SqlError, parse_view, viewplan};

/// One `SELECT … FROM t WHERE …` as the `IDX.QUERY` argv that answers
/// it over `t`'s declared paths. Values are literals; a `$N` slot is a
/// card parameter and is refused here (that is `sql compile`'s shape).
/// A query no declared path serves is refused with the same text
/// `sql plan` gives it. Never an engine view: a view is a declaration,
/// and this answers a query now.
///
/// ```
/// let orders: Vec<String> = "TABLE.DECLARE orders PREFIX orders: PK id COLUMN id i64 \
///     COLUMN user_id i64 COLUMN total f64 INDEX user_id range VALUES total"
///     .split_whitespace()
///     .map(String::from)
///     .collect();
/// let card = kevy_sql::select_card(&[orders.clone()], "SELECT id FROM orders WHERE user_id = 7").unwrap();
/// assert_eq!(card.argv.join(" "), "IDX.QUERY orders.user_id EQ 7 FIELDS id");
///
/// let refused = kevy_sql::select_card(&[orders], "SELECT id FROM orders WHERE total >= 1").unwrap_err();
/// assert!(refused.message.contains("total"), "{refused}");
/// ```
pub fn select_card(declarations: &[Vec<String>], select: &str) -> Result<QueryCard, SqlError> {
    let v = parse_one_select(select)?;
    let tables = declarations
        .iter()
        .map(|argv| declared::read(argv).map_err(|e| SqlError::at(v.line, v.col, e)))
        .collect::<Result<Vec<Declared>, SqlError>>()?;
    let Some(d) = tables.iter().find(|d| d.table.name == v.table) else {
        return Err(SqlError::at(
            v.line,
            v.col,
            format!(
                "FROM unknown table '{}' \u{2014} TABLE.LIST enumerates the declared tables",
                v.table
            ),
        ));
    };
    let card = viewplan::card_for(&v, &d.table)?;
    if let Some(p) = card.params.first() {
        return Err(SqlError::at(
            v.line,
            v.col,
            format!(
                "${} is a query-card parameter \u{2014} sql run takes literal values; `sql compile` makes cards",
                p.n
            ),
        ));
    }
    Ok(card)
}

fn parse_one_select(select: &str) -> Result<CreateView, SqlError> {
    let toks = lex(select)?;
    let mut p = P::new(&toks);
    let t = p.peek();
    let (line, col) = (t.line, t.col);
    p.expect_kw("select", "\u{2014} sql run takes one SELECT")?;
    let v = parse_view::parse_select_body(&mut p, "select".into(), line, col)?;
    while p.eat_sym(';') {}
    if !matches!(p.peek().tok, Tok::Eof) {
        return Err(
            p.err_here("expected the end of the SELECT \u{2014} sql run takes one statement")
        );
    }
    Ok(v)
}

/// A `TABLE.DECLARE` argv as the SQL `sql compile` turns back into the
/// same declaration: `CREATE TABLE`, then one `CREATE [UNIQUE] INDEX` per
/// index and order path, in declaration order. What SQL has no words for
/// — a key prefix other than `<table>:`, `WINDOW`, `AUTODECLARE`, a
/// one-column order path — is written as a `--` comment naming the
/// clause, so a reader sees what the SQL form does not carry.
///
/// ```
/// let decl: Vec<String> = "TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 \
///     COLUMN email str INDEX email unique"
///     .split_whitespace()
///     .map(String::from)
///     .collect();
/// let ddl = kevy_sql::table_ddl(&decl).unwrap();
/// assert_eq!(
///     ddl,
///     "CREATE TABLE users (\n    id bigint PRIMARY KEY,\n    email text\n);\n\
///      CREATE UNIQUE INDEX ON users (email);\n"
/// );
/// assert_eq!(kevy_sql::compile(&ddl).unwrap().commands, vec![decl]);
/// ```
pub fn table_ddl(declaration: &[String]) -> Result<String, String> {
    let d = declared::read(declaration)?;
    let t = &d.table;
    let mut out = String::new();
    let mut lost: Vec<String> = Vec::new();
    if d.prefix != format!("{}:", t.name) {
        lost.push(format!("PREFIX {}", d.prefix));
    }
    out.push_str(&format!("CREATE TABLE {} (\n", ident(&t.name)?));
    let mut lines = Vec::new();
    for (c, ty) in &t.columns {
        let pk = if *c == t.pk { " PRIMARY KEY" } else { "" };
        lines.push(format!("    {} {}{pk}", ident(c)?, sql_type(*ty)));
    }
    out.push_str(&lines.join(",\n"));
    out.push_str("\n);\n");
    indexes(t, &mut out)?;
    orderpaths(t, &mut out, &mut lost)?;
    lost.extend(d.beyond_sql.iter().map(|words| words.join(" ")));
    for clause in lost {
        out.push_str(&format!("-- not carried by SQL: {clause}\n"));
    }
    Ok(out)
}

fn indexes(t: &Table, out: &mut String) -> Result<(), String> {
    for ix in &t.indexes {
        let unique = if ix.unique { "UNIQUE " } else { "" };
        out.push_str(&format!(
            "CREATE {unique}INDEX ON {} ({})",
            ident(&t.name)?,
            ident(&ix.column)?
        ));
        if !ix.values.is_empty() {
            out.push_str(&format!(" INCLUDE ({})", idents(&ix.values)?));
        }
        out.push_str(";\n");
    }
    Ok(())
}

/// A composite index per order path. SQL reads a one-column index as a
/// plain index, so a one-column order path is noted instead.
fn orderpaths(t: &Table, out: &mut String, lost: &mut Vec<String>) -> Result<(), String> {
    for op in &t.orderpaths {
        if let [(c, true)] = op.on.as_slice() {
            // One descending column reads back as the same order path.
            let stmt = format!(
                "CREATE INDEX {} ON {} ({} DESC);\n",
                ident(&op.name)?,
                ident(&t.name)?,
                ident(c)?
            );
            out.push_str(&stmt);
            continue;
        }
        if op.on.len() < 2 {
            let on: Vec<String> = op
                .on
                .iter()
                .map(|(c, desc)| if *desc { format!("{c} DESC") } else { c.clone() })
                .collect();
            lost.push(format!("ORDERPATH {} ON {}", op.name, on.join(" THEN ")));
            continue;
        }
        let cols = op
            .on
            .iter()
            .map(|(c, desc)| Ok(format!("{}{}", ident(c)?, if *desc { " DESC" } else { "" })))
            .collect::<Result<Vec<String>, String>>()?
            .join(", ");
        out.push_str(&format!(
            "CREATE INDEX {} ON {} ({cols});\n",
            ident(&op.name)?,
            ident(&t.name)?
        ));
    }
    Ok(())
}

fn sql_type(ty: KevyType) -> &'static str {
    match ty {
        KevyType::I64 => "bigint",
        KevyType::F64 => "double precision",
        KevyType::Str => "text",
    }
}

/// A name as the lexer reads it back: bare when it is already a
/// lower-case identifier, `"quoted"` otherwise. A `"` inside a name has
/// no spelling in this dialect, so it is refused rather than mangled.
fn ident(name: &str) -> Result<String, String> {
    let bare = name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if bare {
        Ok(name.to_string())
    } else if name.contains('"') {
        Err(format!("the name '{name}' contains '\"', which has no SQL spelling here"))
    } else {
        Ok(format!("\"{name}\""))
    }
}

fn idents(names: &[String]) -> Result<String, String> {
    Ok(names.iter().map(|n| ident(n)).collect::<Result<Vec<String>, String>>()?.join(", "))
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
