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
    ///
    /// ```
    /// let decl: Vec<String> = ["TABLE.LIST"].iter().map(|w| w.to_string()).collect();
    /// let e = kevy_sql::table_ddl(&decl).unwrap_err();
    /// assert_eq!(e, kevy_sql::DeclarationError::NotADeclaration("TABLE.LIST".into()));
    /// assert_eq!(e.to_string(), "not a TABLE.DECLARE declaration: TABLE.LIST");
    /// ```
    NotADeclaration(String),
    /// A clause that ends before its arguments do; carries its keyword.
    ///
    /// ```
    /// let decl: Vec<String> =
    ///     "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN n".split(' ').map(String::from).collect();
    /// let e = kevy_sql::table_ddl(&decl).unwrap_err();
    /// assert_eq!(e, kevy_sql::DeclarationError::CutShort("COLUMN".into()));
    /// assert_eq!(e.to_string(), "COLUMN is cut short");
    /// ```
    CutShort(String),
    /// A column type other than i64, f64 or str; carries it lowercased.
    ///
    /// ```
    /// let decl: Vec<String> =
    ///     "TABLE.DECLARE t PREFIX t: PK id COLUMN id BLOB".split(' ').map(String::from).collect();
    /// let e = kevy_sql::table_ddl(&decl).unwrap_err();
    /// assert_eq!(e, kevy_sql::DeclarationError::ColumnType("blob".into()));
    /// assert_eq!(e.to_string(), "column type 'blob' is not i64|f64|str");
    /// ```
    ColumnType(String),
    /// A word where a clause keyword belongs.
    ///
    /// ```
    /// let decl: Vec<String> =
    ///     "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 FROB".split(' ').map(String::from).collect();
    /// let e = kevy_sql::table_ddl(&decl).unwrap_err();
    /// assert_eq!(e, kevy_sql::DeclarationError::UnknownClause("FROB".into()));
    /// assert_eq!(e.to_string(), "unknown clause 'FROB'");
    /// ```
    UnknownClause(String),
    /// An `ORDERPATH` without `<name> ON <col>`.
    ///
    /// ```
    /// let decl: Vec<String> =
    ///     "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH p AT id".split(' ').map(String::from).collect();
    /// let e = kevy_sql::table_ddl(&decl).unwrap_err();
    /// assert_eq!(e, kevy_sql::DeclarationError::OrderpathShape);
    /// assert_eq!(e.to_string(), "ORDERPATH needs <name> ON <col>");
    /// ```
    OrderpathShape,
    /// A name containing `"`, which this SQL dialect cannot quote.
    ///
    /// ```
    /// let decl: Vec<String> =
    ///     "TABLE.DECLARE t\"x PREFIX t: PK id COLUMN id i64".split(' ').map(String::from).collect();
    /// let e = kevy_sql::table_ddl(&decl).unwrap_err();
    /// assert_eq!(e, kevy_sql::DeclarationError::Unspellable("t\"x".into()));
    /// assert_eq!(e.to_string(), "the name 't\"x' contains '\"', which has no SQL spelling here");
    /// ```
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
