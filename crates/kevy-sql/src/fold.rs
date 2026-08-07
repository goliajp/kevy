//! `fold` — evaluate a table-free `SELECT` to literal rows.
//!
//! The V1 scalar-function face (RFC 2026-08-08): `SELECT f(args…)`
//! with no FROM clause is a pure expression, and the whole probe
//! corpus' scalar block has exactly that shape. Evaluation delegates
//! to kevy-scalar; this module owns only the SYNTAX — function-call
//! grammar, literals, `::` casts, `INTERVAL` literals, the
//! `extract(field FROM x)` / `position(a IN b)` special forms, and
//! `+ - * /`.
//!
//! The clock never reaches kevy-scalar: `now()` / `current_timestamp`
//! / `current_date` are rewritten HERE to the literal the caller
//! supplies (the probe-08 precedent — spg pins its test clock the
//! same way). A statement with FROM/WHERE/anything wider is a named
//! refusal, not an error of this module's business: the caller routes
//! those to the plan face.

use crate::lex::{Tok, Token, lex};
use crate::{SqlError, fold_parse};
use kevy_scalar::Scalar;

/// One folded statement: the column values of the single result row.
#[derive(Debug)]
pub struct Folded {
    /// One rendered value per SELECT column, PG text form; `None` is
    /// SQL NULL (the caller decides its marker).
    pub columns: Vec<Option<String>>,
}

/// Evaluate a table-free `SELECT expr[, expr…];` against the given
/// clock (epoch microseconds). Refusals are [`SqlError`]s whose text
/// names the construct — never a silent wrong answer.
pub fn fold_select(sql: &str, now_micros: i64) -> Result<Folded, SqlError> {
    let toks = lex(sql)?;
    let mut p = Parser { t: &toks, i: 0, now: now_micros };
    p.expect_kw("select")?;
    let mut columns = Vec::new();
    loop {
        let v = p.expr()?;
        columns.push(if v.is_null() { None } else { Some(v.render()) });
        if !p.eat_sym(',') {
            break;
        }
    }
    p.eat_sym(';');
    let t = p.peek();
    if !matches!(t.tok, Tok::Eof) {
        return Err(SqlError::at(
            t.line,
            t.col,
            match &t.tok {
                Tok::Ident(w) if w == "from" => {
                    "FROM is not foldable — a table query compiles through the plan face; \
                     only table-free SELECT folds here"
                        .to_string()
                }
                other => format!("unexpected {other:?} after the select list"),
            },
        ));
    }
    Ok(Folded { columns })
}

pub(crate) struct Parser<'a> {
    pub(crate) t: &'a [Token],
    pub(crate) i: usize,
    pub(crate) now: i64,
}

impl<'a> Parser<'a> {
    pub(crate) fn peek(&self) -> &'a Token {
        &self.t[self.i.min(self.t.len() - 1)]
    }

    pub(crate) fn bump(&mut self) -> &'a Token {
        let t = &self.t[self.i.min(self.t.len() - 1)];
        self.i += 1;
        t
    }

    pub(crate) fn eat_sym(&mut self, c: char) -> bool {
        if matches!(self.peek().tok, Tok::Sym(s) if s == c) {
            self.i += 1;
            return true;
        }
        false
    }

    pub(crate) fn eat_kw(&mut self, kw: &str) -> bool {
        if matches!(&self.peek().tok, Tok::Ident(w) if w == kw) {
            self.i += 1;
            return true;
        }
        false
    }

    pub(crate) fn expect_kw(&mut self, kw: &str) -> Result<(), SqlError> {
        let t = self.peek();
        if self.eat_kw(kw) {
            return Ok(());
        }
        Err(SqlError::at(t.line, t.col, format!("expected {}", kw.to_uppercase())))
    }

    pub(crate) fn expect_sym(&mut self, c: char) -> Result<(), SqlError> {
        let t = self.peek();
        if self.eat_sym(c) {
            return Ok(());
        }
        Err(SqlError::at(t.line, t.col, format!("expected '{c}'")))
    }

    /// `expr := term (('+'|'-') term)*` — evaluated left to right;
    /// precedence with `* /` handled one level down.
    pub(crate) fn expr(&mut self) -> Result<Scalar, SqlError> {
        let mut acc = self.term()?;
        loop {
            let t = self.peek();
            let op = match t.tok {
                Tok::Sym(c @ ('+' | '-')) => c,
                _ => return Ok(acc),
            };
            let (line, col) = (t.line, t.col);
            self.i += 1;
            let rhs = self.term()?;
            acc = kevy_scalar::binop(op, &acc, &rhs)
                .map_err(|e| SqlError::at(line, col, e.to_string()))?;
        }
    }

    fn term(&mut self) -> Result<Scalar, SqlError> {
        let mut acc = fold_parse::primary(self)?;
        loop {
            let t = self.peek();
            let op = match t.tok {
                Tok::Sym(c @ ('*' | '/')) => c,
                _ => return Ok(acc),
            };
            let (line, col) = (t.line, t.col);
            self.i += 1;
            let rhs = fold_parse::primary(self)?;
            acc = kevy_scalar::binop(op, &acc, &rhs)
                .map_err(|e| SqlError::at(line, col, e.to_string()))?;
        }
    }
}
