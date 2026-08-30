//! The pg_dump dialect — everything a real `pg_dump --schema-only`
//! writes that a hand-written schema does not (the V2 drill's third
//! wall, hit on the drill's own seed database):
//!
//!   * `SET …` preamble and `SELECT pg_catalog.set_config(…)` — the
//!     session setup pg_dump emits before any DDL. Skipped: they carry
//!     no schema meaning here.
//!   * `ALTER TABLE … OWNER TO …` — skipped, no ownership layer.
//!   * `ALTER TABLE ONLY t ADD CONSTRAINT n PRIMARY KEY (col)` — the
//!     load-bearing one: pg_dump NEVER writes inline PKs, so without
//!     this fold-back every dumped table would refuse for "no PRIMARY
//!     KEY". UNIQUE folds the same way; FOREIGN KEY / CHECK become
//!     honest notes (the NOT NULL precedent — unenforceable, said,
//!     not fatal).
//!   * `public.` schema qualification — stripped; any OTHER schema is
//!     refused by name (dump one schema at a time).

use crate::SqlError;
use crate::ast::{AlterConstraint, AlterKind, Stmt};
use crate::lex::Tok;
use crate::parse::P;

/// The dialect's skippable / foldable statements, tried before the
/// refusal table: `None` = not a dialect statement; `Some(Ok(None))`
/// = fully consumed, nothing to keep.
pub(crate) fn dump_statement(p: &mut P<'_>, verb: &str) -> Option<Result<Option<Stmt>, SqlError>> {
    match verb {
        "set" => {
            skip_set(p);
            Some(Ok(None))
        }
        "select" if is_set_config(p) => {
            skip_set(p);
            Some(Ok(None))
        }
        "alter" => Some(parse_alter(p)),
        _ => None,
    }
}

/// `SET <anything> ;` — consume and drop.
pub(crate) fn skip_set(p: &mut P<'_>) {
    while !matches!(p.peek().tok, Tok::Sym(';') | Tok::Eof) {
        p.bump();
    }
}

/// Top-level `SELECT`: pg_dump's `SELECT pg_catalog.set_config(…)` is
/// preamble and skips; any other top-level SELECT keeps its refusal.
pub(crate) fn is_set_config(p: &P<'_>) -> bool {
    // Lookahead: select pg_catalog . set_config
    matches!(&p.peek_at(1).tok, Tok::Ident(w) if w == "pg_catalog")
        && matches!(&p.peek_at(2).tok, Tok::Sym('.'))
        && matches!(&p.peek_at(3).tok, Tok::Ident(w) if w == "set_config")
}

/// A possibly schema-qualified name: `users` or `public.users`. The
/// `public` prefix strips; anything else refuses by name.
pub(crate) fn qualified_name(p: &mut P<'_>, what: &str) -> Result<(String, u32, u32), SqlError> {
    let (first, line, col) = p.ident(what)?;
    if !matches!(p.peek().tok, Tok::Sym('.')) {
        return Ok((first, line, col));
    }
    p.bump();
    let (second, l2, c2) = p.ident("a name after the schema qualifier")?;
    if first != "public" {
        return Err(SqlError::at(
            line,
            col,
            format!(
                "schema '{first}' is not compilable — kevy has one keyspace; \
                 dump one schema at a time (pg_dump --schema=public)"
            ),
        ));
    }
    Ok((second, l2, c2))
}

/// `ALTER TABLE [ONLY] <t> …` — the three dump forms.
pub(crate) fn parse_alter(p: &mut P<'_>) -> Result<Option<Stmt>, SqlError> {
    let t = p.peek();
    let (line, col) = (t.line, t.col);
    p.bump(); // alter
    p.expect_kw("table", "after ALTER")?;
    if p.is_kw("only") {
        p.bump();
    }
    let (table, ..) = qualified_name(p, "a table name")?;
    if p.is_kw("owner") {
        skip_set(p); // `OWNER TO x` — no ownership layer; drop the rest.
        return Ok(None);
    }
    if !p.is_kw("add") {
        return Err(SqlError::at(
            line,
            col,
            "ALTER is not compilable — declarations compile once; \
             TABLE.DROP, edit the schema, re-apply (only pg_dump's \
             ADD CONSTRAINT forms fold back)"
                .to_string(),
        ));
    }
    p.bump();
    if !p.is_kw("constraint") {
        // `ADD COLUMN` and friends keep the pre-dialect lesson.
        return Err(SqlError::at(
            line,
            col,
            "ALTER is not compilable — declarations compile once; \
             TABLE.DROP, edit the schema, re-apply (only pg_dump's \
             ADD CONSTRAINT forms fold back)"
                .to_string(),
        ));
    }
    p.bump();
    let _ = p.ident("a constraint name")?;
    let kind = parse_constraint_kind(p, line, col)?;
    Ok(Some(Stmt::Alter(AlterConstraint { table, kind, line, col })))
}

fn parse_constraint_kind(p: &mut P<'_>, line: u32, col: u32) -> Result<AlterKind, SqlError> {
    if p.is_kw("primary") {
        p.bump();
        p.expect_kw("key", "after PRIMARY")?;
        return Ok(AlterKind::PrimaryKey(single_column(p)?));
    }
    if p.is_kw("unique") {
        p.bump();
        return Ok(AlterKind::Unique(single_column(p)?));
    }
    if p.is_kw("foreign") {
        skip_set(p);
        return Ok(AlterKind::Noted(
            "FOREIGN KEY constraint dropped — kevy enforces no constraints (Law 3); \
             keep the FK as an indexed column (cookbook §10)",
        ));
    }
    if p.is_kw("check") {
        skip_set(p);
        return Ok(AlterKind::Noted(
            "CHECK constraint dropped — the atomic-block recipe (cookbook §5)",
        ));
    }
    Err(SqlError::at(line, col, "unsupported ADD CONSTRAINT form".to_string()))
}

/// `(col)` — exactly one column (composite keys keep their existing
/// refusal at the fold-back site in schema build).
fn single_column(p: &mut P<'_>) -> Result<String, SqlError> {
    p.expect_sym('(', "before the constraint column")?;
    let (col, line, c) = p.ident("a column name")?;
    if matches!(p.peek().tok, Tok::Sym(',')) {
        return Err(SqlError::at(
            line,
            c,
            "composite constraint — kevy keys are single-column; concatenate \
             app-side or re-model (cookbook §2)"
                .to_string(),
        ));
    }
    p.expect_sym(')', "after the constraint column")?;
    Ok(col)
}
