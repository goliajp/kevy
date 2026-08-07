//! The hand-written recursive-descent parser (0-dep, charter): exactly
//! the declaration subset parses; **everything else errors by name**,
//! with line/column, and the refusal teaches the kevy-shaped
//! alternative instead of just saying no.

use crate::ast::{ColumnDef, CreateIndex, CreateTable, Stmt};
use crate::lex::{Tok, Token};
use crate::{KevyType, SqlError, typemap};

/// Token cursor. Never advances past `Eof`.
pub(crate) struct P<'a> {
    toks: &'a [Token],
    i: usize,
}

impl<'a> P<'a> {
    pub(crate) fn peek(&self) -> &'a Token {
        &self.toks[self.i]
    }

    /// Lookahead without consuming: token at `i + n` (clamped to Eof).
    pub(crate) fn peek_at(&self, n: usize) -> &'a Token {
        &self.toks[(self.i + n).min(self.toks.len() - 1)]
    }

    pub(crate) fn bump(&mut self) -> &'a Token {
        let t = &self.toks[self.i];
        if !matches!(t.tok, Tok::Eof) {
            self.i += 1;
        }
        t
    }

    /// Is the current token the (unquoted, case-folded) keyword `kw`?
    pub(crate) fn is_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().tok, Tok::Ident(s) if s == kw)
    }

    pub(crate) fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect_kw(&mut self, kw: &str, ctx: &str) -> Result<(), SqlError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err_here(format!("expected {} {ctx}", kw.to_ascii_uppercase())))
        }
    }

    pub(crate) fn is_sym(&self, ch: char) -> bool {
        matches!(&self.peek().tok, Tok::Sym(c) if *c == ch)
    }

    pub(crate) fn eat_sym(&mut self, ch: char) -> bool {
        if self.is_sym(ch) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect_sym(&mut self, ch: char, ctx: &str) -> Result<(), SqlError> {
        if self.eat_sym(ch) {
            Ok(())
        } else {
            Err(self.err_here(format!("expected '{ch}' {ctx}")))
        }
    }

    /// An identifier (unquoted → already lower-cased, or `"quoted"`),
    /// with its anchor.
    pub(crate) fn ident(&mut self, what: &str) -> Result<(String, u32, u32), SqlError> {
        let t = self.peek();
        match &t.tok {
            Tok::Ident(s) => {
                let out = (s.clone(), t.line, t.col);
                self.bump();
                Ok(out)
            }
            Tok::QIdent(s) => {
                let out = (s.clone(), t.line, t.col);
                self.bump();
                Ok(out)
            }
            _ => Err(self.err_here(format!("expected {what}"))),
        }
    }

    pub(crate) fn err_here(&self, msg: impl Into<String>) -> SqlError {
        let t = self.peek();
        SqlError::at(t.line, t.col, msg)
    }

    /// The standard named-refusal shape: `<NAME> is not compilable — <teach>`.
    pub(crate) fn refuse(&self, name: &str, teach: &str) -> SqlError {
        self.err_here(format!("{name} is not compilable \u{2014} {teach}"))
    }
}

/// Parse a whole script into statements.
pub(crate) fn parse_script(toks: &[Token]) -> Result<Vec<Stmt>, SqlError> {
    let mut p = P { toks, i: 0 };
    let mut out = Vec::new();
    loop {
        while p.eat_sym(';') {}
        if matches!(p.peek().tok, Tok::Eof) {
            return Ok(out);
        }
        if let Some(stmt) = parse_statement(&mut p)? {
            out.push(stmt);
        }
    }
}

/// The non-CREATE statement verbs, each refused with its own lesson.
fn refused_verb(p: &P<'_>, verb: &str) -> Option<SqlError> {
    let teach = match verb {
        "insert" | "update" | "delete" => {
            "this compiler emits declarations only; writes go through the live commands (HSET \u{2026}, cookbook \u{a7}1)"
        }
        "select" => {
            "ad-hoc SQL never runs against kevy (Law 3); declare it as CREATE VIEW and use the compiled access path"
        }
        "alter" => "declarations compile once \u{2014} TABLE.DROP, edit the schema, re-apply",
        "drop" => "issue TABLE.DROP / IDX.DROP / VIEW.DROP directly against the server",
        "truncate" => "delete the rows through the live commands (kevy-cli delete-prefix <p>)",
        "with" => {
            "CTEs are query-time composition (Law 3); declare each step as its own view"
        }
        "grant" | "revoke" => "kevy has no SQL privilege layer (AUTH/TLS are permanently out)",
        "begin" | "commit" | "rollback" => {
            "transactions are runtime commands (WATCH/MULTI/EXEC, cookbook \u{a7}4), not declarations"
        }
        "set" | "explain" | "vacuum" | "analyze" | "comment" | "copy" => {
            "it is a session/maintenance statement, not a declaration"
        }
        _ => return None,
    };
    Some(p.refuse(&verb.to_ascii_uppercase(), teach))
}

fn parse_statement(p: &mut P<'_>) -> Result<Option<Stmt>, SqlError> {
    let Tok::Ident(verb) = &p.peek().tok else {
        return Err(p.err_here("expected a statement (CREATE \u{2026})"));
    };
    let verb = verb.clone();
    if let Some(r) = crate::parse_dump::dump_statement(p, &verb) {
        return r;
    }
    if let Some(e) = refused_verb(p, &verb) {
        return Err(e);
    }
    if verb != "create" {
        return Err(p.err_here(format!("expected CREATE, got '{verb}'")));
    }
    p.bump();
    if p.eat_kw("table") {
        return parse_create_table(p).map(Some);
    }
    if p.eat_kw("unique") {
        p.expect_kw("index", "after UNIQUE")?;
        return parse_create_index(p, true).map(Some);
    }
    if p.eat_kw("index") {
        return parse_create_index(p, false).map(Some);
    }
    if p.is_kw("view") {
        p.bump();
        return crate::parse_view::parse_create_view(p).map(Some);
    }
    if p.is_kw("or") {
        return Err(p.refuse(
            "OR REPLACE",
            "declarations compile once; TABLE.DROP / VIEW.DROP the old object, then re-apply",
        ));
    }
    if p.is_kw("materialized") {
        return Err(p.refuse(
            "CREATE MATERIALIZED VIEW",
            "materialization is an engine-side mode \u{2014} declare a plain view here, then add MODE materialized to the emitted VIEW.CREATE by hand (docs/views.md)",
        ));
    }
    if p.is_kw("temporary") || p.is_kw("temp") {
        return Err(p.refuse("TEMPORARY", "declarations are durable catalog objects"));
    }
    Err(p.err_here("expected TABLE, [UNIQUE] INDEX or VIEW after CREATE"))
}

// ───────────── CREATE TABLE ─────────────

fn parse_create_table(p: &mut P<'_>) -> Result<Stmt, SqlError> {
    let (name, line, col) = crate::parse_dump::qualified_name(p, "a table name")?;
    let mut t = CreateTable { name, columns: Vec::new(), pk: None, uniques: Vec::new(), line, col };
    p.expect_sym('(', "to open the column list")?;
    loop {
        parse_table_item(p, &mut t)?;
        if p.eat_sym(',') {
            continue;
        }
        break;
    }
    p.expect_sym(')', "to close the column list")?;
    p.expect_sym(';', "after CREATE TABLE")?;
    Ok(Stmt::Table(t))
}

fn parse_table_item(p: &mut P<'_>, t: &mut CreateTable) -> Result<(), SqlError> {
    if p.is_kw("primary") {
        return parse_table_pk(p, t);
    }
    if p.is_kw("unique") {
        return parse_table_unique(p, t);
    }
    if p.is_kw("foreign") {
        return Err(p.refuse(
            "FOREIGN KEY",
            "kevy enforces no constraints (Law 3); keep the FK as an indexed column and use the cascade recipe for deletes (cookbook \u{a7}10)",
        ));
    }
    if p.is_kw("check") {
        return Err(p.refuse(
            "CHECK",
            "constraints are the atomic-block recipe \u{2014} verify inside WATCH/MULTI (cookbook \u{a7}5)",
        ));
    }
    if p.is_kw("constraint") {
        return Err(p.refuse(
            "CONSTRAINT",
            "named constraints carry CHECK/FK semantics kevy refuses; declare plain UNIQUE (col) / PRIMARY KEY (col) items instead",
        ));
    }
    parse_column_def(p, t)
}

/// Table-level `PRIMARY KEY (<col>)` — single column only.
fn parse_table_pk(p: &mut P<'_>, t: &mut CreateTable) -> Result<(), SqlError> {
    let (line, col) = (p.peek().line, p.peek().col);
    p.bump();
    p.expect_kw("key", "after PRIMARY")?;
    p.expect_sym('(', "after PRIMARY KEY")?;
    let (pk, ..) = p.ident("the primary-key column")?;
    if p.is_sym(',') {
        return Err(p.refuse(
            "a composite PRIMARY KEY",
            "a kevy row has one key; concatenate the parts into the key (cookbook \u{a7}1) and keep each part as a column",
        ));
    }
    p.expect_sym(')', "after the primary-key column")?;
    if t.pk.is_some() {
        return Err(SqlError::at(line, col, "duplicate PRIMARY KEY".to_string()));
    }
    t.pk = Some((pk, line, col));
    Ok(())
}

/// Table-level `UNIQUE (<col>)` — single column only.
fn parse_table_unique(p: &mut P<'_>, t: &mut CreateTable) -> Result<(), SqlError> {
    let (line, col) = (p.peek().line, p.peek().col);
    p.bump();
    p.expect_sym('(', "after UNIQUE")?;
    let (u, ..) = p.ident("the unique column")?;
    if p.is_sym(',') {
        return Err(p.refuse(
            "a multi-column UNIQUE constraint",
            "composite access paths are Range, not Unique; enforce the pair app-side (verify-not-enforce, cookbook \u{a7}6) or concatenate it into one column",
        ));
    }
    p.expect_sym(')', "after the unique column")?;
    t.uniques.push((u, line, col));
    Ok(())
}

/// Consume a `DEFAULT` expression without interpreting it: anything
/// up to the next top-level `,` or `)`. The value is app-side
/// knowledge (the note says so); the parser only needs the boundary.
fn skip_default_expr(p: &mut P<'_>) -> Result<(), SqlError> {
    let mut depth = 0u32;
    loop {
        match &p.peek().tok {
            Tok::Sym('(') => depth += 1,
            Tok::Sym(')') if depth == 0 => return Ok(()),
            Tok::Sym(')') => depth -= 1,
            Tok::Sym(',') if depth == 0 => return Ok(()),
            Tok::Eof => {
                let t = p.peek();
                return Err(SqlError::at(t.line, t.col, "unterminated DEFAULT expression"));
            }
            _ => {}
        }
        p.bump();
    }
}

fn parse_column_def(p: &mut P<'_>, t: &mut CreateTable) -> Result<(), SqlError> {
    let (name, line, col) = p.ident("a column name or a table constraint")?;
    let (ty, sql_ty) = parse_type(p)?;
    let mut def = ColumnDef {
        name,
        ty,
        sql_ty,
        inline_pk: false,
        not_null: false,
        dropped_default: false,
        line,
        col,
    };
    loop {
        if p.is_kw("primary") {
            p.bump();
            p.expect_kw("key", "after PRIMARY")?;
            def.inline_pk = true;
        } else if p.is_kw("not") {
            // Accepted with an honest note, not refused: every real
            // pg_dump writes NOT NULL on nearly every column, and a
            // fatal refusal here walls migration day at the first
            // mile (the V2 drill hit this on its own seed schema).
            p.bump();
            p.expect_kw("null", "after NOT")?;
            def.not_null = true;
        } else if p.is_kw("default") {
            p.bump();
            skip_default_expr(p)?;
            def.dropped_default = true;
        } else if p.is_kw("references") {
            return Err(p.refuse(
                "REFERENCES",
                "kevy enforces no constraints (Law 3); keep the FK as an indexed column (cookbook \u{a7}10)",
            ));
        } else if p.is_kw("unique") {
            return Err(p.refuse(
                "an inline UNIQUE",
                "declare it table-level \u{2014} UNIQUE (<col>) \u{2014} or as CREATE UNIQUE INDEX ON <t> (<col>)",
            ));
        } else if p.is_kw("check") {
            return Err(p.refuse("CHECK", "constraints are the atomic-block recipe (cookbook \u{a7}5)"));
        } else {
            break;
        }
    }
    t.columns.push(def);
    Ok(())
}

/// Parse one SQL type (with optional `(n[, m])` args) and map it.
fn parse_type(p: &mut P<'_>) -> Result<(Option<KevyType>, String), SqlError> {
    let t = p.peek();
    let Tok::Ident(first) = &t.tok else {
        return Err(p.err_here("expected a column type"));
    };
    let mut name = first.clone();
    p.bump();
    if name == "double" {
        p.expect_kw("precision", "after DOUBLE")?;
        name = "double precision".into();
    }
    // pg_dump's canonical spellings: `timestamp without time zone` is
    // plain timestamp; `with time zone` is timestamptz (both map to
    // str, note-carried — the same verdict the short names get).
    if name == "timestamp" && (p.is_kw("with") || p.is_kw("without")) {
        let with = p.is_kw("with");
        p.bump();
        p.expect_kw("time", "in the timestamp type name")?;
        p.expect_kw("zone", "in the timestamp type name")?;
        if with {
            name = "timestamptz".into();
        }
    }
    // Unknown types PARSE — the verdict moves to schema build so the
    // plan face can drop the one table and keep reporting (a pg_dump
    // with one money column must not lose its whole plan). `compile`
    // still fails on it, at the same message, from build_table.
    let ty = typemap::map_type(&name);
    if p.is_sym('(') {
        if ty.is_some() && !typemap::takes_args(&name) {
            return Err(p.err_here(format!("type '{name}' takes no arguments")));
        }
        p.bump();
        parse_type_args(p)?;
    }
    Ok((ty, name))
}

fn parse_type_args(p: &mut P<'_>) -> Result<(), SqlError> {
    let Tok::Num(_) = &p.peek().tok else {
        return Err(p.err_here("expected a numeric precision argument"));
    };
    p.bump();
    if p.eat_sym(',') {
        let Tok::Num(_) = &p.peek().tok else {
            return Err(p.err_here("expected a numeric scale argument"));
        };
        p.bump();
    }
    p.expect_sym(')', "to close the type arguments")?;
    Ok(())
}

// ───────────── CREATE [UNIQUE] INDEX ─────────────

fn parse_create_index(p: &mut P<'_>, unique: bool) -> Result<Stmt, SqlError> {
    let (line, col) = (p.peek().line, p.peek().col);
    let name = if p.is_kw("on") {
        None
    } else {
        let (n, ..) = p.ident("an index name or ON")?;
        Some(n)
    };
    p.expect_kw("on", "in CREATE INDEX")?;
    let (table, ..) = crate::parse_dump::qualified_name(p, "the table name")?;
    if p.is_kw("using") {
        // pg_dump writes `USING btree` on every index — that IS the
        // structure kevy declares, so it passes. Any other method
        // keeps the refusal (gin/gist need a different genre).
        p.bump();
        if p.is_kw("btree") {
            p.bump();
        } else {
            return Err(p.refuse(
                "USING <method>",
                "the declared kind picks the structure \u{2014} single column = Range (UNIQUE = Unique), multi-column = a composite ORDERPATH; text search is the FT.* genre",
            ));
        }
    }
    p.expect_sym('(', "to open the index column list")?;
    let mut cols = Vec::new();
    loop {
        let (c, ..) = p.ident("an index column")?;
        let desc = parse_direction(p)?;
        cols.push((c, desc));
        if p.eat_sym(',') {
            continue;
        }
        break;
    }
    p.expect_sym(')', "to close the index column list")?;
    let include = parse_include(p)?;
    if p.is_kw("where") {
        return Err(p.refuse(
            "a partial index (WHERE)",
            "declare a flag column and a view over it instead (soft-delete recipe, cookbook \u{a7}7)",
        ));
    }
    p.expect_sym(';', "after CREATE INDEX")?;
    Ok(Stmt::Index(CreateIndex { unique, name, table, cols, include, line, col }))
}

fn parse_direction(p: &mut P<'_>) -> Result<bool, SqlError> {
    if p.eat_kw("asc") {
        return Ok(false);
    }
    if p.eat_kw("desc") {
        return Ok(true);
    }
    if p.is_kw("nulls") {
        return Err(p.refuse(
            "NULLS FIRST/LAST",
            "NULL is an absent field and absent fields leave the index entirely \u{2014} there is no NULL placement to choose",
        ));
    }
    Ok(false)
}

fn parse_include(p: &mut P<'_>) -> Result<Vec<String>, SqlError> {
    if !p.eat_kw("include") {
        return Ok(Vec::new());
    }
    p.expect_sym('(', "after INCLUDE")?;
    let mut cols = Vec::new();
    loop {
        let (c, ..) = p.ident("an INCLUDE column")?;
        cols.push(c);
        if p.eat_sym(',') {
            continue;
        }
        break;
    }
    p.expect_sym(')', "to close the INCLUDE list")?;
    Ok(cols)
}
