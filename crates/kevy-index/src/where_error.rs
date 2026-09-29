//! Why a composite `WHERE` was refused.

use std::fmt;

use super::show;
use crate::catalog::ValType;

/// Why [`crate::composite_bounds`] refused a `WHERE`: the words name the
/// bound and the composite's declared columns, and carry no wire code
/// (the caller prefixes the command and index it answers for).
///
/// ```
/// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
///
/// let cols = [CompositeCol::new("a", ValType::I64)];
/// let argv: Vec<Vec<u8>> = ["b", "EQ", "1"].iter().map(|s| s.as_bytes().to_vec()).collect();
/// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
/// let e = composite_bounds(&cols, &w, 0).unwrap_err();
/// assert!(matches!(e, WhereError::UnknownColumn { .. }));
/// assert_eq!(e.to_string(), "WHERE names column 'b', which this composite does not declare — it declares: a");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WhereError {
    /// A bound starting `@` that is not a time expression.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
    /// let cols = [CompositeCol::new("at", ValType::I64)];
    /// let argv = [b"at".to_vec(), b"EQ".to_vec(), b"@soon".to_vec()];
    /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
    /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
    /// assert_eq!(e, WhereError::TimeExpression { bound: b"@soon".to_vec(), column: b"at".to_vec() });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    TimeExpression {
        /// The bound as written.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("at", ValType::I64)];
        /// let argv = [b"RANGE".to_vec(), b"at".to_vec(), b"@now-1d".to_vec(), b"@later".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// assert!(matches!(e, WhereError::TimeExpression { bound, .. } if bound == b"@later"));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        bound: Vec<u8>,
        /// The column it bounds.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("day", ValType::I64)];
        /// let argv = [b"day".to_vec(), b"EQ".to_vec(), b"@yesterday".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// assert!(matches!(e, WhereError::TimeExpression { column, .. } if column == b"day"));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        column: Vec<u8>,
    },
    /// A bound that is not a value of its column's declared type.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
    /// let cols = [CompositeCol::new("n", ValType::I64)];
    /// let argv = [b"n".to_vec(), b"EQ".to_vec(), b"ten".to_vec()];
    /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
    /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
    /// assert_eq!(e.to_string(), "WHERE bound 'ten' is not a valid i64, which is how this composite declares 'n'");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Value {
        /// The bound as written.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("price", ValType::F64)];
        /// let argv = [b"RANGE".to_vec(), b"price".to_vec(), b"1.5".to_vec(), b"lots".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// assert!(matches!(e, WhereError::Value { bound, .. } if bound == b"lots"));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        bound: Vec<u8>,
        /// The column's declared type.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("price", ValType::F64)];
        /// let argv = [b"price".to_vec(), b"EQ".to_vec(), b"free".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// assert!(matches!(e, WhereError::Value { ty: ValType::F64, .. }));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        ty: ValType,
        /// The column it bounds.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("city", ValType::Str), CompositeCol::new("n", ValType::I64)];
        /// let argv: Vec<Vec<u8>> =
        ///     ["city", "EQ", "kyoto", "n", "EQ", "x"].iter().map(|s| s.as_bytes().to_vec()).collect();
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// assert!(matches!(e, WhereError::Value { column, .. } if column == b"n"));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        column: Vec<u8>,
    },
    /// A `WHERE` column the composite does not declare.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
    /// let cols = [CompositeCol::new("a", ValType::I64)];
    /// let argv = [b"z".to_vec(), b"EQ".to_vec(), b"1".to_vec()];
    /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
    /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
    /// assert_eq!(e, WhereError::UnknownColumn { column: b"z".to_vec(), declared: vec![b"a".to_vec()] });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    UnknownColumn {
        /// The column `WHERE` names.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("a", ValType::I64)];
        /// let argv = [b"RANGE".to_vec(), b"b".to_vec(), b"0".to_vec(), b"9".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// assert!(matches!(e, WhereError::UnknownColumn { column, .. } if column == b"b"));
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        column: Vec<u8>,
        /// The composite's columns, in declared order.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("a", ValType::I64), CompositeCol::new("b", ValType::Str)];
        /// let argv = [b"c".to_vec(), b"EQ".to_vec(), b"1".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// let WhereError::UnknownColumn { declared, .. } = e else { panic!("{e}") };
        /// assert_eq!(declared, [b"a".to_vec(), b"b".to_vec()]);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        declared: Vec<Vec<u8>>,
    },
    /// `WHERE` columns that are not a leading prefix of the declared
    /// order.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
    /// let cols = [CompositeCol::new("a", ValType::I64), CompositeCol::new("b", ValType::I64)];
    /// let argv = [b"b".to_vec(), b"EQ".to_vec(), b"1".to_vec()];
    /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
    /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
    /// assert!(matches!(e, WhereError::NotLeadingPrefix { .. }));
    /// assert_eq!(e.to_string(), "WHERE columns must be a leading prefix of the composite's declared order (a, b)");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    NotLeadingPrefix {
        /// The composite's columns, in declared order.
        ///
        /// ```
        /// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
        /// let cols = [CompositeCol::new("a", ValType::I64), CompositeCol::new("b", ValType::I64)];
        /// let argv = [b"RANGE".to_vec(), b"b".to_vec(), b"0".to_vec(), b"9".to_vec()];
        /// let (w, _) = parse_where(&argv, 0, |_| false).ok_or("not a WHERE")?;
        /// let e = composite_bounds(&cols, &w, 0).unwrap_err();
        /// let WhereError::NotLeadingPrefix { declared } = e else { panic!("{e}") };
        /// assert_eq!(declared, [b"a".to_vec(), b"b".to_vec()]);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        declared: Vec<Vec<u8>>,
    },
}

struct List<'a>(&'a [Vec<u8>]);

impl fmt::Display for List<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&show(c))?;
        }
        Ok(())
    }
}

impl fmt::Display for WhereError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimeExpression { bound, column } => write!(
                f,
                "WHERE bound '{}' is not a valid time expression for '{}'",
                show(bound),
                show(column)
            ),
            Self::Value { bound, ty, column } => write!(
                f,
                "WHERE bound '{}' is not a valid {}, which is how this composite declares '{}'",
                show(bound),
                ty.tag(),
                show(column)
            ),
            Self::UnknownColumn { column, declared } => write!(
                f,
                "WHERE names column '{}', which this composite does not declare — it declares: {}",
                show(column),
                List(declared)
            ),
            Self::NotLeadingPrefix { declared } => write!(
                f,
                "WHERE columns must be a leading prefix of the composite's declared order ({})",
                List(declared)
            ),
        }
    }
}

impl std::error::Error for WhereError {}
