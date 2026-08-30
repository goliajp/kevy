//! Predicate normalization: SQL comparisons → per-column `EQ` / `RANGE`
//! shapes with typed, engine-literal bounds. Everything here is
//! mechanical encoding, never planning — and every lossy step (strict
//! bounds on non-integers, open str ranges) is a named error rather
//! than a silent approximation, because `RANGE` bounds are inclusive.

use crate::ast::{Bound, CreateView, Pred, PredOp};
use crate::schema::Table;
use crate::{KevyType, SqlError};

/// A bound value in argv form: a literal (engine-literal text) or a
/// `$N` slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BV {
    Lit(String),
    Param(u32),
}

/// One merged per-column predicate shape.
#[derive(Debug, Clone)]
pub(crate) enum Shape {
    Eq(BV),
    /// Inclusive `[lo, hi]` — SQL `>=`/`<=`/`BETWEEN` map exactly;
    /// strict `>`/`<` adjust integer literals by one (exact) and error
    /// on anything else.
    Range(BV, BV),
}

/// One column's merged predicate.
#[derive(Debug, Clone)]
pub(crate) struct ColPred {
    pub(crate) col: String,
    pub(crate) ty: KevyType,
    pub(crate) shape: Shape,
}

impl ColPred {
    pub(crate) fn has_param(&self) -> bool {
        match &self.shape {
            Shape::Eq(b) => matches!(b, BV::Param(_)),
            Shape::Range(lo, hi) => matches!(lo, BV::Param(_)) || matches!(hi, BV::Param(_)),
        }
    }
}

#[derive(Default)]
struct Acc {
    eq: Option<BV>,
    lo: Option<BV>,
    hi: Option<BV>,
}

/// Merge the view's predicates per column (first-appearance order),
/// type-checking every literal against the declared column type.
pub(crate) fn normalize(v: &CreateView, t: &Table) -> Result<Vec<ColPred>, SqlError> {
    let mut cols: Vec<(String, KevyType, Acc)> = Vec::new();
    for p in &v.preds {
        let Some(ty) = t.column_type(&p.column) else {
            return Err(SqlError::at(
                p.line,
                p.col,
                format!(
                    "view '{}': WHERE names unknown column '{}' of table '{}'",
                    v.name, p.column, t.name
                ),
            ));
        };
        let acc = match cols.iter_mut().find(|(c, ..)| c == &p.column) {
            Some((_, _, a)) => a,
            None => {
                cols.push((p.column.clone(), ty, Acc::default()));
                &mut cols.last_mut().expect("just pushed").2
            }
        };
        apply(acc, p, ty)?;
    }
    cols.into_iter().map(|(col, ty, acc)| finish(v, col, ty, acc)).collect()
}

/// Fold one predicate into its column's accumulator.
fn apply(acc: &mut Acc, p: &Pred, ty: KevyType) -> Result<(), SqlError> {
    let clash = |what: &str| {
        SqlError::at(
            p.line,
            p.col,
            format!(
                "predicates on '{}' do not combine \u{2014} one EQ, or one range ({what} is already set)",
                p.column
            ),
        )
    };
    let a = typed(p, &p.a, ty)?;
    match p.op {
        PredOp::Eq => {
            if acc.eq.is_some() || acc.lo.is_some() || acc.hi.is_some() {
                return Err(clash("a bound"));
            }
            acc.eq = Some(a);
        }
        PredOp::Ge | PredOp::Gt => {
            if acc.eq.is_some() || acc.lo.is_some() {
                return Err(clash("a lower bound"));
            }
            acc.lo = Some(if p.op == PredOp::Gt { strict(p, a, ty, 1)? } else { a });
        }
        PredOp::Le | PredOp::Lt => {
            if acc.eq.is_some() || acc.hi.is_some() {
                return Err(clash("an upper bound"));
            }
            acc.hi = Some(if p.op == PredOp::Lt { strict(p, a, ty, -1)? } else { a });
        }
        PredOp::Between => {
            if acc.eq.is_some() || acc.lo.is_some() || acc.hi.is_some() {
                return Err(clash("a bound"));
            }
            acc.lo = Some(a);
            acc.hi = Some(typed(p, p.b.as_ref().expect("BETWEEN has b"), ty)?);
        }
    }
    Ok(())
}

/// Type-check one bound against the declared column type; literals pass
/// through as engine-literal argv text.
fn typed(p: &Pred, b: &Bound, ty: KevyType) -> Result<BV, SqlError> {
    let err = |msg: String| SqlError::at(p.line, p.col, msg);
    match (b, ty) {
        (Bound::Param(n), _) => Ok(BV::Param(*n)),
        (Bound::Num(s), KevyType::I64) => {
            if s.parse::<i64>().is_err() {
                return Err(err(format!(
                    "'{s}' is not an i64 \u{2014} which is how column '{}' is declared",
                    p.column
                )));
            }
            Ok(BV::Lit(s.clone()))
        }
        (Bound::Num(s), KevyType::F64) => {
            if s.parse::<f64>().is_err() {
                return Err(err(format!("'{s}' is not an f64")));
            }
            Ok(BV::Lit(s.clone()))
        }
        (Bound::Str(s), KevyType::Str) => Ok(BV::Lit(s.clone())),
        (Bound::Str(s), _) => Err(err(format!(
            "column '{}' is {} \u{2014} write a number literal, not '{s}'",
            p.column,
            ty.tag()
        ))),
        (Bound::Num(s), KevyType::Str) => {
            Err(err(format!("column '{}' is str \u{2014} quote the literal ('{s}')", p.column)))
        }
    }
}

/// Strict `>` / `<`: exact only for integer literals (±1); anything
/// else is a named refusal, because `RANGE` bounds are inclusive and a
/// silent off-by-epsilon would be an approximation, not a compilation.
fn strict(p: &Pred, b: BV, ty: KevyType, delta: i64) -> Result<BV, SqlError> {
    let opname = if delta > 0 { ">" } else { "<" };
    match (&b, ty) {
        (BV::Lit(s), KevyType::I64) => {
            let n = s.parse::<i64>().expect("typed() validated");
            let adj = n.checked_add(delta).ok_or_else(|| {
                SqlError::at(p.line, p.col, format!("'{opname} {s}' overflows i64"))
            })?;
            Ok(BV::Lit(adj.to_string()))
        }
        _ => Err(SqlError::at(
            p.line,
            p.col,
            format!(
                "strict '{opname}' on a {} bound is not exactly compilable \u{2014} RANGE bounds are inclusive; use >= / <= / BETWEEN",
                match &b {
                    BV::Param(_) => "parameter".to_string(),
                    BV::Lit(_) => ty.tag().to_string(),
                }
            ),
        )),
    }
}

/// Close the accumulator into a shape, filling open range ends with the
/// type's representable extremes (str has no finite upper bound — named
/// error).
fn finish(v: &CreateView, col: String, ty: KevyType, acc: Acc) -> Result<ColPred, SqlError> {
    if let Some(eq) = acc.eq {
        return Ok(ColPred { col, ty, shape: Shape::Eq(eq) });
    }
    let lo = acc.lo.unwrap_or_else(|| {
        BV::Lit(
            match ty {
                KevyType::I64 => "-9223372036854775808",
                KevyType::F64 => "-inf",
                KevyType::Str => "",
            }
            .to_string(),
        )
    });
    let hi = match acc.hi {
        Some(h) => h,
        None => match ty {
            KevyType::I64 => BV::Lit("9223372036854775807".into()),
            KevyType::F64 => BV::Lit("inf".into()),
            KevyType::Str => {
                return Err(SqlError::at(
                    v.line,
                    v.col,
                    format!(
                        "view '{}': an open-ended range on the str column '{col}' has no finite upper bound \u{2014} use BETWEEN with an explicit upper sentinel (cookbook \u{a7}8)",
                        v.name
                    ),
                ));
            }
        },
    };
    Ok(ColPred { col, ty, shape: Shape::Range(lo, hi) })
}
