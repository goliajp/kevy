//! `ConfigError` — the error type [`crate::Config::load`] /
//! [`crate::Config::from_toml_str`] / `merge_env` / `merge_cli` return.
//! Lifted out of `schema.rs` to keep that file under the 500-LOC
//! house rule; the schema module now stays focused on the data
//! definitions while this one owns the failure surface.

use std::path::PathBuf;

/// Reasons `Config::load` / `from_toml_str` can fail.
///
/// ```
/// use kevy_config::{Config, ConfigError};
///
/// let e = Config::from_toml_str("[memory]\nmaxmemory_policy = \"sometimes\"\n", None).unwrap_err();
/// assert!(matches!(e, ConfigError::Schema { line: 2, .. }));
/// assert!(e.to_string().starts_with("kevy-config: schema error at line 2"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConfigError {
    /// File could not be opened or read.
    ///
    /// ```
    /// use kevy_config::{Config, ConfigError};
    /// use std::path::Path;
    ///
    /// let missing = Path::new("/nonexistent/kevy.toml");
    /// let Err(ConfigError::IoOpen { path, err }) = Config::load(Some(missing)) else {
    ///     panic!("a missing explicit file is an error");
    /// };
    /// assert_eq!(path, missing);
    /// assert!(!err.is_empty());
    /// ```
    IoOpen {
        /// Path that failed to open.
        ///
        /// ```
        /// let e = kevy_config::Config::load(Some("/nonexistent/kevy.toml".as_ref())).unwrap_err();
        /// let kevy_config::ConfigError::IoOpen { path, .. } = e else { unreachable!() };
        /// assert_eq!(path.to_str(), Some("/nonexistent/kevy.toml"));
        /// ```
        path: PathBuf,
        /// Underlying error message.
        ///
        /// ```
        /// let e = kevy_config::Config::load(Some("/nonexistent/kevy.toml".as_ref())).unwrap_err();
        /// let kevy_config::ConfigError::IoOpen { err, .. } = e else { unreachable!() };
        /// assert!(!err.is_empty()); // the OS error text
        /// ```
        err: String,
    },
    /// Tokenizer / parser error with line + column.
    ///
    /// ```
    /// use kevy_config::{Config, ConfigError};
    ///
    /// let Err(ConfigError::Parse { line, col, msg }) = Config::from_toml_str("[server]\nport 7000\n", None)
    /// else {
    ///     panic!("a key without `=` does not parse");
    /// };
    /// assert_eq!(line, 2);
    /// assert!(col >= 1 && !msg.is_empty());
    /// ```
    Parse {
        /// 1-based line number in the source.
        ///
        /// ```
        /// let e = kevy_config::Config::from_toml_str("# header\n\n[server\n", None).unwrap_err();
        /// let kevy_config::ConfigError::Parse { line, .. } = e else { unreachable!() };
        /// assert_eq!(line, 3);
        /// ```
        line: usize,
        /// 1-based column number in the source.
        ///
        /// ```
        /// let e = kevy_config::Config::from_toml_str("port 7000\n", None).unwrap_err();
        /// let kevy_config::ConfigError::Parse { col, .. } = e else { unreachable!() };
        /// assert!(col >= 1);
        /// ```
        col: usize,
        /// Human-readable error.
        ///
        /// ```
        /// let e = kevy_config::Config::from_toml_str("[server\n", None).unwrap_err();
        /// let kevy_config::ConfigError::Parse { msg, .. } = e else { unreachable!() };
        /// assert!(!msg.is_empty());
        /// ```
        msg: String,
    },
    /// Value passed schema validation but the field rejected it
    /// (e.g. unknown enum variant, out-of-range integer).
    ///
    /// ```
    /// use kevy_config::{Config, ConfigError};
    ///
    /// let Err(ConfigError::Schema { line, field, msg }) =
    ///     Config::from_toml_str("[server]\nport = 70000\n", None)
    /// else {
    ///     panic!("a port past u16 is refused by the schema");
    /// };
    /// assert_eq!((line, field.as_str()), (2, "[server].port"));
    /// assert!(msg.contains("70000"));
    /// ```
    Schema {
        /// 1-based line number where the offending value appeared.
        ///
        /// ```
        /// let e = kevy_config::Config::from_toml_str("[server]\nport = 6004\nthreads = -1\n", None).unwrap_err();
        /// let kevy_config::ConfigError::Schema { line, .. } = e else { unreachable!() };
        /// assert_eq!(line, 3);
        /// ```
        line: usize,
        /// `[section].key` of the rejected setting.
        ///
        /// ```
        /// let e = kevy_config::Config::from_toml_str("[log]\nlevel = \"loud\"\n", None).unwrap_err();
        /// let kevy_config::ConfigError::Schema { field, .. } = e else { unreachable!() };
        /// assert_eq!(field, "[log].level");
        /// ```
        field: String,
        /// Human-readable error.
        ///
        /// ```
        /// let e = kevy_config::Config::from_toml_str("[log]\nlevel = \"loud\"\n", None).unwrap_err();
        /// let kevy_config::ConfigError::Schema { msg, .. } = e else { unreachable!() };
        /// assert!(msg.contains("trace"), "the message lists what is accepted: {msg}");
        /// ```
        msg: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoOpen { path, err } => {
                write!(f, "kevy-config: cannot read {}: {err}", path.display())
            }
            Self::Parse { line, col, msg } => {
                write!(f, "kevy-config: parse error at line {line} col {col}: {msg}")
            }
            Self::Schema { line, field, msg } => {
                write!(f, "kevy-config: schema error at line {line} on {field}: {msg}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// A single value's text was refused — a size literal, a key, a budget,
/// a peer or scope token, a notification flag string. The message says
/// what was wrong and quotes the offending text.
///
/// ```
/// let e = kevy_config::parse_size("12qb").unwrap_err();
/// assert!(e.to_string().contains("unknown unit"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValueError {
    msg: String,
}

impl ValueError {
    pub(crate) fn new(msg: impl Into<String>) -> Self {
        Self { msg: msg.into() }
    }
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ValueError {}
