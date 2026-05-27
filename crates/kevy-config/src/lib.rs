//! `kevy-config` — TOML subset parser + Config schema for the kevy server.
//!
//! Zero crates.io dependencies; `#![forbid(unsafe_code)]`. Built specifically
//! for kevy's config file shape; not a general-purpose TOML library.
//!
//! Supported TOML subset:
//! - `[section]` table headers (one level deep)
//! - `key = value` with `value` ∈ {string, integer, boolean, size literal}
//! - String literals: `"double-quoted"` and `'single-quoted'`
//! - Integers: signed decimal (`123`, `-7`); prefixed forms (0x/0o/0b) NOT supported
//! - Booleans: `true` / `false`
//! - Size literals (kevy-specific extension): `"64mb"` / `"2gb"` / `"512kb"`
//!   parsed via [`parse_size`] when the schema field expects bytes
//! - `# comment` to end of line
//!
//! INTENTIONALLY UNSUPPORTED (TOML spec features kevy doesn't need):
//! - dotted keys (`a.b.c = ...`)
//! - multi-line strings (`"""…"""`)
//! - arrays / arrays of tables
//! - inline tables (`{ a = 1, b = 2 }`)
//! - datetime literals
//!
//! See [`Config`] for the schema, [`Config::load`] for the precedence chain.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod apply;
mod lex;
mod parse;
mod schema;
mod size;

pub use schema::{
    AppendFsync, Config, ConfigError, EvictionPolicy, ExpirySection, LogLevel,
    LogOutput, LogSection, MemorySection, PersistenceSection, ServerSection,
};
pub use size::parse_size;

use std::path::{Path, PathBuf};

/// Auto-detect search order when `Config::load(None)` is called.
const AUTODETECT_PATHS: &[&str] = &[
    "./kevy.toml",
    "/etc/kevy/kevy.toml",
];

impl Config {
    /// Load config from the given explicit path, or auto-detect.
    ///
    /// Auto-detect order (first hit wins):
    /// 1. `$KEVY_DIR/kevy.toml` (if `KEVY_DIR` env is set)
    /// 2. `./kevy.toml`
    /// 3. `/etc/kevy/kevy.toml`
    ///
    /// If `path` is `Some`, that file is required to exist; otherwise
    /// returns `Ok(Config::default())` if no auto-detect path matched.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        if let Some(p) = path {
            let text = read_required(p)?;
            return Self::from_toml_str(&text, Some(p));
        }
        if let Some(p) = autodetect() {
            let text = read_required(&p)?;
            let mut cfg = Self::from_toml_str(&text, Some(&p))?;
            cfg.source_path = Some(p);
            return Ok(cfg);
        }
        Ok(Self::default())
    }

    /// Parse a TOML string (no file I/O). `source_path` is used for error
    /// reporting and `CONFIG REWRITE` write-back; pass `None` for in-memory.
    pub fn from_toml_str(text: &str, source_path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut cfg = Self::default();
        let items = parse::parse(text)?;
        for item in items {
            cfg.apply_item(item)?;
        }
        if let Some(p) = source_path {
            cfg.source_path = Some(p.to_path_buf());
        }
        Ok(cfg)
    }

    /// Overlay environment variable values onto `self`. Iterates a
    /// caller-provided `(name, value)` list so tests can pump a fixture
    /// without touching the real env. The recognised variables match the
    /// pre-`kevy-config` set:
    /// - `KEVY_BIND` / `KEVY_PORT` / `KEVY_THREADS` / `KEVY_DIR` / `KEVY_AOF`
    ///
    /// Unknown variables are silently ignored (env may contain many
    /// unrelated keys).
    pub fn merge_env<I, K, V>(&mut self, env: I) -> Result<(), ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (k, v) in env {
            self.apply_env_var(k.as_ref(), v.as_ref())?;
        }
        Ok(())
    }

    /// Overlay parsed-from-CLI overrides onto `self`. Pass a struct of
    /// optional values (any `Some(_)` field overrides the corresponding
    /// schema field). Tests pass a literal; the kevy binary builds one
    /// from `std::env::args`.
    pub fn merge_cli(&mut self, cli: CliOverrides) -> Result<(), ConfigError> {
        if let Some(bind) = cli.bind {
            self.server.bind = bind;
        }
        if let Some(port) = cli.port {
            self.server.port = port;
        }
        if let Some(t) = cli.threads {
            self.server.threads = t;
        }
        if let Some(d) = cli.data_dir {
            self.server.data_dir = d;
        }
        if let Some(aof) = cli.aof {
            self.persistence.aof = aof;
        }
        Ok(())
    }
}

/// Optional CLI overrides applied via [`Config::merge_cli`].
///
/// Any `Some(_)` field overrides the corresponding schema field. CLI is the
/// highest-priority source (above env vars and the TOML file).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct CliOverrides {
    /// Override `server.bind` (`--bind A.B.C.D`).
    pub bind: Option<[u8; 4]>,
    /// Override `server.port` (`--port N`).
    pub port: Option<u16>,
    /// Override `server.threads` (`--threads N`).
    pub threads: Option<usize>,
    /// Override `server.data_dir` (`--dir PATH`).
    pub data_dir: Option<PathBuf>,
    /// Override `persistence.aof` (`--no-aof` → `Some(false)`).
    pub aof: Option<bool>,
}

fn read_required(p: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(p).map_err(|e| ConfigError::IoOpen {
        path: p.to_path_buf(),
        err: e.to_string(),
    })
}

fn autodetect() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("KEVY_DIR") {
        let p = PathBuf::from(dir).join("kevy.toml");
        if p.exists() {
            return Some(p);
        }
    }
    for relative in AUTODETECT_PATHS {
        let p = PathBuf::from(relative);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        let cfg = Config::default();
        assert_eq!(cfg.server.bind, [127, 0, 0, 1]);
        assert_eq!(cfg.server.port, 6004);
        assert_eq!(cfg.server.threads, 0);
        assert!(cfg.persistence.aof);
        assert_eq!(cfg.persistence.appendfsync, AppendFsync::EverySec);
        assert_eq!(cfg.memory.maxmemory, 0);
        assert_eq!(cfg.memory.maxmemory_policy, EvictionPolicy::NoEviction);
        assert_eq!(cfg.expiry.hz, 10);
        assert_eq!(cfg.expiry.sample, 20);
        assert_eq!(cfg.log.level, LogLevel::Info);
    }

    #[test]
    fn cli_overrides_apply_in_order() {
        let mut cfg = Config::default();
        let cli = CliOverrides {
            bind: Some([0, 0, 0, 0]),
            port: Some(7000),
            threads: Some(4),
            ..CliOverrides::default()
        };
        cfg.merge_cli(cli).unwrap();
        assert_eq!(cfg.server.bind, [0, 0, 0, 0]);
        assert_eq!(cfg.server.port, 7000);
        assert_eq!(cfg.server.threads, 4);
    }

    #[test]
    fn env_overrides_apply() {
        let mut cfg = Config::default();
        cfg.merge_env([
            ("KEVY_BIND", "0.0.0.0"),
            ("KEVY_PORT", "7001"),
            ("UNRELATED_VAR", "ignored"),
        ])
        .unwrap();
        assert_eq!(cfg.server.bind, [0, 0, 0, 0]);
        assert_eq!(cfg.server.port, 7001);
    }
}
