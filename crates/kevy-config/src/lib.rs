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
mod cluster;
mod error;
mod lex;
mod parse;
mod preserve;
mod replication;
mod schema;
mod size;

pub use cluster::{ClusterSection, PeerEntry};
pub use replication::{ReplicationRole, ReplicationSection};
pub use schema::{
    AdvancedSection, AppendFsync, Config, ConfigError, EvictionPolicy,
    ExpirySection, LogLevel, LogOutput, LogSection, MemorySection, NotificationFlags,
    NotificationSection, PersistenceSection, ServerSection, SlowlogSection,
    parse_notification_flags,
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
        if let Some(cluster) = cli.cluster {
            self.cluster.enabled = cluster;
        }
        Ok(())
    }

    /// Render the current config as a standard-template TOML file —
    /// every field, in stable section/key order, with no comments. Used
    /// by `CONFIG REWRITE`; the loss of any inline comments the user
    /// had in their hand-edited file is the documented v1.0 trade-off
    /// (v1.x will preserve them).
    ///
    /// Round-trips: feeding the output back through [`Self::from_toml_str`]
    /// reconstructs an equivalent `Config` (modulo `source_path`).
    pub fn to_toml_string(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let [a, b, c, d] = self.server.bind;
        let _ = writeln!(out, "[server]");
        let _ = writeln!(out, "bind     = \"{a}.{b}.{c}.{d}\"");
        let _ = writeln!(out, "port     = {}", self.server.port);
        let _ = writeln!(out, "threads  = {}", self.server.threads);
        let _ = writeln!(
            out,
            "data_dir = \"{}\"",
            escape_toml_basic_string(&self.server.data_dir.display().to_string()),
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[persistence]");
        let _ = writeln!(out, "aof                          = {}", self.persistence.aof);
        let _ = writeln!(
            out,
            "appendfsync                  = \"{}\"",
            self.persistence.appendfsync.as_str(),
        );
        let _ = writeln!(
            out,
            "auto_aof_rewrite_percentage  = {}",
            self.persistence.auto_aof_rewrite_percentage,
        );
        let _ = writeln!(
            out,
            "auto_aof_rewrite_min_size    = {}",
            self.persistence.auto_aof_rewrite_min_size,
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[memory]");
        let _ = writeln!(out, "maxmemory         = {}", self.memory.maxmemory);
        let _ = writeln!(
            out,
            "maxmemory_policy  = \"{}\"",
            self.memory.maxmemory_policy.as_str(),
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[expiry]");
        let _ = writeln!(out, "hz       = {}", self.expiry.hz);
        let _ = writeln!(out, "sample   = {}", self.expiry.sample);
        let _ = writeln!(out);
        let _ = writeln!(out, "[log]");
        let _ = writeln!(out, "level    = \"{}\"", self.log.level.as_str());
        let _ = writeln!(
            out,
            "output   = \"{}\"",
            escape_toml_basic_string(&self.log.output.as_str()),
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[notification]");
        let _ = writeln!(
            out,
            "notify_keyspace_events = \"{}\"",
            escape_toml_basic_string(&self.notification.notify_keyspace_events),
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[advanced]");
        let _ = writeln!(out, "spin_limit       = {}", self.advanced.spin_limit);
        let _ = writeln!(out, "park_timeout_ms  = {}", self.advanced.park_timeout_ms);
        let _ = writeln!(out, "tick_check_every = {}", self.advanced.tick_check_every);
        let _ = writeln!(out, "ring_capacity    = {}", self.advanced.ring_capacity);
        let _ = writeln!(out);
        let _ = writeln!(out, "[slowlog]");
        let _ = writeln!(
            out,
            "slower_than_micros = {}",
            self.slowlog.slower_than_micros,
        );
        let _ = writeln!(out, "max_len            = {}", self.slowlog.max_len);
        out
    }
}

/// Escape a string for use inside a TOML basic (double-quoted) string.
/// `\` and `"` need backslash escape; other ASCII passes through. The
/// values we emit (paths, enum names) never contain control characters,
/// so this is sufficient for our serialiser.
fn escape_toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
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
    /// Override `cluster.enabled` (`--cluster` → `Some(true)`).
    pub cluster: Option<bool>,
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

    #[test]
    fn to_toml_string_round_trips_through_parser() {
        let mut original = Config::default();
        original.server.bind = [10, 0, 0, 1];
        original.server.port = 7779;
        original.server.threads = 4;
        original.server.data_dir = PathBuf::from("/var/lib/kevy");
        original.persistence.aof = false;
        original.persistence.appendfsync = AppendFsync::Always;
        original.persistence.auto_aof_rewrite_percentage = 200;
        original.persistence.auto_aof_rewrite_min_size = 128 * 1024 * 1024;
        original.memory.maxmemory = 4 * 1024 * 1024 * 1024;
        original.memory.maxmemory_policy = EvictionPolicy::AllKeysLfu;
        original.expiry.hz = 100;
        original.expiry.sample = 50;
        original.log.level = LogLevel::Warn;
        original.log.output = LogOutput::File(PathBuf::from("/var/log/kevy.log"));

        let toml_text = original.to_toml_string();
        let mut reparsed = Config::from_toml_str(&toml_text, None).unwrap_or_else(|e| {
            panic!("to_toml_string output did not reparse: {e}\n--- TOML ---\n{toml_text}")
        });
        // Re-parsing sets source_path only when one is passed; the live
        // config's source_path is not part of the wire format.
        reparsed.source_path = original.source_path.clone();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn to_toml_string_escapes_quotes_and_backslashes_in_paths() {
        let mut cfg = Config::default();
        cfg.server.data_dir = PathBuf::from(r#"/path with "quote" and \back"#);
        let text = cfg.to_toml_string();
        assert!(
            text.contains(r#"data_dir = "/path with \"quote\" and \\back""#),
            "did not escape correctly: {text}"
        );
        // Round-trip the escape.
        let reparsed = Config::from_toml_str(&text, None).expect("reparse");
        assert_eq!(reparsed.server.data_dir, cfg.server.data_dir);
    }
}
