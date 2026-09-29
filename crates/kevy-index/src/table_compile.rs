//! Admission and compilation of a [`TableSpec`]: the per-clause
//! validation [`TableSpec::validate`] runs, and the single compiler both
//! the server and the embedded store install.

use super::{TableSpec, dotted};
use crate::catalog::{IndexKind, ValType};
use crate::composite::{CompositeCol, MAX_COMPOSITE_COLS};
use crate::spec::IndexSpec;
use crate::spec_parts::ValueSpec;
use crate::table_error::TableError;

impl TableSpec {
    /// The window needs an i64 column, positive span/bucket with
    /// bucket <= span, and an access path whose tree tail can answer
    /// max(column) for free: a single-column INDEX on it, or an
    /// ORDERPATH whose FIRST column is it, ascending.
    pub(super) fn validate_window(&self) -> Result<(), TableError> {
        let Some(w) = &self.window else { return Ok(()) };
        match self.column_type(&w.column) {
            None => return Err(TableError::WindowUnknownColumn(w.column.clone())),
            Some(ValType::I64) => {}
            Some(_) => return Err(TableError::WindowColumnType),
        }
        if w.span <= 0 || w.bucket <= 0 {
            return Err(TableError::WindowNotPositive);
        }
        if w.bucket > w.span {
            return Err(TableError::WindowBucketExceedsSpan);
        }
        let indexed = self.indexes.iter().any(|ix| ix.column == w.column);
        let leads_path = self.orderpaths.iter().any(|op| op.led_ascending_by(&w.column));
        if !indexed && !leads_path {
            return Err(TableError::WindowNeedsPath(w.column.clone()));
        }
        Ok(())
    }

    pub(super) fn validate_columns_and_pk(&self) -> Result<(), TableError> {
        for (i, (name, ty)) in self.columns.iter().enumerate() {
            if !matches!(ty, ValType::I64 | ValType::F64 | ValType::Str) {
                return Err(TableError::ColumnType);
            }
            if self.columns[..i].iter().any(|(n, _)| n == name) {
                return Err(TableError::DuplicateColumn(name.clone()));
            }
        }
        if self.column_type(&self.pk).is_none() {
            return Err(TableError::PkUndeclared(self.pk.clone()));
        }
        Ok(())
    }

    pub(super) fn validate_indexes(&self) -> Result<(), TableError> {
        for (i, ix) in self.indexes.iter().enumerate() {
            if !matches!(ix.kind, IndexKind::Range | IndexKind::Unique) {
                return Err(TableError::IndexKind);
            }
            if self.column_type(&ix.column).is_none() {
                return Err(TableError::IndexUnknownColumn(ix.column.clone()));
            }
            if self.indexes[..i].iter().any(|p| p.column == ix.column) {
                return Err(TableError::DuplicateIndex(ix.column.clone()));
            }
            for v in &ix.values {
                if self.column_type(v).is_none() {
                    return Err(TableError::ValuesUnknownColumn(v.clone()));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_orderpaths(&self) -> Result<(), TableError> {
        for (i, op) in self.orderpaths.iter().enumerate() {
            if op.on.is_empty() {
                return Err(TableError::OrderpathNeedsOn);
            }
            if op.on.len() > MAX_COMPOSITE_COLS {
                return Err(TableError::OrderpathTooManyColumns);
            }
            if self.orderpaths[..i].iter().any(|p| p.name == op.name) {
                return Err(TableError::DuplicateOrderpath(op.name.clone()));
            }
            // The compiled names share one namespace: `<table>.<col>`
            // vs `<table>.<orderpath>` colliding would be two indexes
            // with one name — refused here, by name, not downstream.
            if self.indexes.iter().any(|ix| ix.column == op.name) {
                return Err(TableError::OrderpathCollides(op.name.clone()));
            }
            for (col, _) in &op.on {
                if self.column_type(col).is_none() {
                    return Err(TableError::OrderpathUnknownColumn {
                        path: op.name.clone(),
                        column: col.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn compile_table(t: &TableSpec) -> Result<Vec<IndexSpec>, TableError> {
    t.validate()?;
    let col_ty = |col: &[u8]| {
        // Post-validate this is total; the Err arm is the honest form
        // of what `expect` asserted, kept reachable so a validate()
        // gap can never again become a panic.
        t.column_type(col).ok_or_else(|| TableError::ColumnUndeclared(col.to_vec()))
    };
    let mut out = Vec::with_capacity(t.indexes.len() + t.orderpaths.len());
    for ix in &t.indexes {
        let ty = col_ty(&ix.column)?;
        let values = ix
            .values
            .iter()
            .map(|c| Ok(ValueSpec::new(c.clone()).with_type(col_ty(c)?)))
            .collect::<Result<_, TableError>>()?;
        let spec = IndexSpec::builder(dotted(&t.name, &ix.column), t.prefix.clone(), ix.kind, ty)
            .with_field(ix.column.clone())
            .with_values(values);
        out.push(spec.build()?);
    }
    for op in &t.orderpaths {
        let cols = op
            .on
            .iter()
            .map(|(col, order)| Ok(CompositeCol::new(col.clone(), col_ty(col)?).with_order(*order)))
            .collect::<Result<_, TableError>>()?;
        let spec = IndexSpec::builder(
            dotted(&t.name, &op.name),
            t.prefix.clone(),
            IndexKind::Range,
            ValType::Str,
        )
        .with_field(op.on[0].0.clone())
        .with_composite(cols);
        out.push(spec.build()?);
    }
    Ok(out)
}
