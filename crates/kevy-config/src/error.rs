//! `ConfigError` — the error type [`crate::Config::load`] /
//! [`crate::Config::from_toml_str`] / `merge_env` / `merge_cli` return.
//! Lifted out of `schema.rs` to keep that file under the 500-LOC
//! house rule; the schema module now stays focused on the data
//! definitions while this one owns the failure surface.

use std::path::PathBuf;

/// Reasons `Config::load` / `from_toml_str` can fail.
#[derive(Debug)]
pub enum ConfigError {
    /// File could not be opened or read.
    IoOpen {
        /// Path that failed to open.
        path: PathBuf,
        /// Underlying error message.
        err: String,
    },
    /// Tokenizer / parser error with line + column.
    Parse {
        /// 1-based line number in the source.
        line: usize,
        /// 1-based column number in the source.
        col: usize,
        /// Human-readable error.
        msg: String,
    },
    /// Value passed schema validation but the field rejected it
    /// (e.g. unknown enum variant, out-of-range integer).
    Schema {
        /// 1-based line number where the offending value appeared.
        line: usize,
        /// `[section].key` of the rejected setting.
        field: String,
        /// Human-readable error.
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
