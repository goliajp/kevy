//! Why a `TABLE.DECLARE` argv could not be read back or rendered as SQL.

use std::fmt;

/// Why [`crate::table_ddl`] refused a declaration: it does not read as a
/// `TABLE.DECLARE`, or it names something SQL text cannot spell.
///
/// ```
/// let decl: Vec<String> = ["TABLE.LIST"].iter().map(|w| w.to_string()).collect();
/// let e = kevy_sql::table_ddl(&decl).unwrap_err();
/// assert!(matches!(e, kevy_sql::DeclarationError::NotADeclaration(_)));
/// assert_eq!(e.to_string(), "not a TABLE.DECLARE declaration: TABLE.LIST");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeclarationError {
    /// The argv does not open with `TABLE.DECLARE name PREFIX p PK col`;
    /// carries the argv joined by spaces.
    NotADeclaration(String),
    /// A clause that ends before its arguments do; carries its keyword.
    CutShort(String),
    /// A column type other than i64, f64 or str; carries it lowercased.
    ColumnType(String),
    /// A word where a clause keyword belongs.
    UnknownClause(String),
    /// An `ORDERPATH` without `<name> ON <col>`.
    OrderpathShape,
    /// A name containing `"`, which this SQL dialect cannot quote.
    Unspellable(String),
}

impl fmt::Display for DeclarationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotADeclaration(argv) => write!(f, "not a TABLE.DECLARE declaration: {argv}"),
            Self::CutShort(word) => write!(f, "{word} is cut short"),
            Self::ColumnType(ty) => write!(f, "column type '{ty}' is not i64|f64|str"),
            Self::UnknownClause(word) => write!(f, "unknown clause '{word}'"),
            Self::OrderpathShape => f.write_str("ORDERPATH needs <name> ON <col>"),
            Self::Unspellable(name) => {
                write!(f, "the name '{name}' contains '\"', which has no SQL spelling here")
            }
        }
    }
}

impl std::error::Error for DeclarationError {}
