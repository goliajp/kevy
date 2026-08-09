//! `fold` — evaluate a table-free `SELECT` to literal rows.
//!
//! The V1 scalar-function face: `SELECT f(args…)`
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
/// Typed, not pre-rendered — the CLI prints PG text forms while the
/// probe runner follows sqllogictest's conventions (booleans as 1/0),
/// and neither should pay for the other's rendering.
#[derive(Debug)]
pub struct Folded {
    /// One value per SELECT column.
    pub columns: Vec<kevy_scalar::Scalar>,
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
        columns.push(p.expr()?);
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

    /// `expr := and_expr (OR and_expr)*` — the top of the operator
    /// pyramid (PG precedence: OR < AND < NOT < comparison < `+ -` <
    /// `* /`), three-valued logic in kevy-scalar.
    pub(crate) fn expr(&mut self) -> Result<Scalar, SqlError> {
        let mut acc = self.and_expr()?;
        while self.eat_kw("or") {
            let rhs = self.and_expr()?;
            acc = kevy_scalar::logic_or(&acc, &rhs)
                .map_err(|e| SqlError::at(0, 0, e.to_string()))?;
        }
        Ok(acc)
    }

    fn and_expr(&mut self) -> Result<Scalar, SqlError> {
        let mut acc = self.not_expr()?;
        while self.eat_kw("and") {
            let rhs = self.not_expr()?;
            acc = kevy_scalar::logic_and(&acc, &rhs)
                .map_err(|e| SqlError::at(0, 0, e.to_string()))?;
        }
        Ok(acc)
    }

    fn not_expr(&mut self) -> Result<Scalar, SqlError> {
        if self.eat_kw("not") {
            let v = self.not_expr()?;
            return kevy_scalar::logic_not(&v).map_err(|e| SqlError::at(0, 0, e.to_string()));
        }
        self.cmp_expr()
    }

    /// One optional comparison: `add (op add)?` — SQL comparisons do
    /// not chain (`a < b < c` is a type error in PG too). The lexer
    /// already folds the two-character forms into single Op tokens.
    fn cmp_expr(&mut self) -> Result<Scalar, SqlError> {
        let lhs = self.add_expr()?;
        let t = self.peek();
        let (line, col) = (t.line, t.col);
        let op: &str = match t.tok {
            Tok::Op(o @ ("=" | "<" | ">" | "<=" | ">=" | "<>" | "!=")) => o,
            _ => return Ok(lhs),
        };
        self.i += 1;
        let rhs = self.add_expr()?;
        kevy_scalar::cmp_op(op, &lhs, &rhs).map_err(|e| SqlError::at(line, col, e.to_string()))
    }

    /// `add_expr := concat (('+'|'-') concat)*` — evaluated left to
    /// right; `||` binds tighter (PG precedence), `* /` one level below.
    fn add_expr(&mut self) -> Result<Scalar, SqlError> {
        let mut acc = self.concat_expr()?;
        loop {
            let t = self.peek();
            let op = match t.tok {
                Tok::Sym(c @ ('+' | '-')) => c,
                _ => return Ok(acc),
            };
            let (line, col) = (t.line, t.col);
            self.i += 1;
            let rhs = self.concat_expr()?;
            acc = kevy_scalar::binop(op, &acc, &rhs)
                .map_err(|e| SqlError::at(line, col, e.to_string()))?;
        }
    }

    /// `concat := term ('||' term)*` — SQL string concatenation with
    /// strict NULL propagation ('a' || NULL is NULL, unlike concat()).
    fn concat_expr(&mut self) -> Result<Scalar, SqlError> {
        let mut acc = self.term()?;
        while matches!(self.peek().tok, Tok::Op("||")) {
            self.i += 1;
            let rhs = self.term()?;
            acc = match (&acc, &rhs) {
                (Scalar::Null, _) | (_, Scalar::Null) => Scalar::Null,
                (a, b) => Scalar::Text(format!("{}{}", a.render(), b.render())),
            };
        }
        Ok(acc)
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
