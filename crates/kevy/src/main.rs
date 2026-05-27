//! kevy server entry point.
//!
//! Reads config in precedence order (top wins): CLI flags → env vars
//! → TOML file (auto-detected) → built-in defaults. See
//! [`kevy_config`] for the schema.
#![forbid(unsafe_code)]

use std::path::PathBuf;

use kevy_config::{CliOverrides, Config};

fn main() -> ! {
    let (config_path, cli) = parse_cli();
    let mut cfg = Config::load(config_path.as_deref()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    cfg.merge_env(env_vars()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    cfg.merge_cli(cli).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let threads = if cfg.server.threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        cfg.server.threads
    };
    let [a, b, c, d] = cfg.server.bind;
    eprintln!(
        "kevy v{} starting: {a}.{b}.{c}.{d}:{}, {threads} shard(s), dir={}, aof={} (thread-per-core)",
        env!("CARGO_PKG_VERSION"),
        cfg.server.port,
        cfg.server.data_dir.display(),
        if cfg.persistence.aof { "on" } else { "off" }
    );
    if !is_loopback(cfg.server.bind) {
        warn_unprotected_bind(cfg.server.bind);
    }
    kevy::serve(
        cfg.server.bind,
        cfg.server.port,
        threads,
        cfg.server.data_dir,
        cfg.persistence.aof,
    ); // never returns
}

/// Parse CLI into `(--config PATH, CliOverrides)`. Backward-compatible with
/// the pre-`kevy-config` flag set: `--bind`, `--port`, `--threads`, `--dir`,
/// `--no-aof` all still work and override env + file values.
fn parse_cli() -> (Option<PathBuf>, CliOverrides) {
    let config_path = arg_value("--config").map(PathBuf::from);
    let aof = if std::env::args().any(|a| a == "--no-aof") {
        Some(false)
    } else {
        None
    };
    let overrides = CliOverrides {
        bind: arg_value("--bind").and_then(|s| parse_ipv4(&s)),
        port: arg_value("--port").and_then(|s| s.parse().ok()),
        threads: arg_value("--threads")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0),
        data_dir: arg_value("--dir").map(PathBuf::from),
        aof,
    };
    (config_path, overrides)
}

/// Snapshot the process env as `(String, String)` pairs for
/// `Config::merge_env`. We materialize because `Config::merge_env`
/// expects an owned iterator (so tests can fixture an in-memory map
/// without touching the global env).
fn env_vars() -> impl IntoIterator<Item = (String, String)> {
    std::env::vars().collect::<Vec<_>>()
}

/// Find `--flag value` or `--flag=value` in the args.
fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    let eq_prefix = format!("{flag}=");
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next();
        }
        if let Some(v) = arg.strip_prefix(&eq_prefix) {
            return Some(v.to_string());
        }
    }
    None
}

/// Parse a dotted-quad IPv4 string into four octets.
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = s.split('.');
    for slot in &mut octets {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

/// `127.0.0.0/8` is the loopback range (RFC 1122). Anything else (a public
/// IP, a LAN address, or the wildcard `0.0.0.0`) is reachable from at
/// least one other host on the network.
#[inline]
fn is_loopback(bind: [u8; 4]) -> bool {
    bind[0] == 127
}

/// Valkey/Redis "protected-mode" style advisory. kevy has no auth yet
/// (deferred to v0.3+); the only safe deployment for a non-loopback bind
/// is a trust-bounded network (docker-compose internal, kubernetes pod
/// network, VPC private subnet). For public exposure, front with
/// stunnel/nginx + IP allowlist until AUTH lands.
fn warn_unprotected_bind(bind: [u8; 4]) {
    let [a, b, c, d] = bind;
    eprintln!("kevy WARN: bind={a}.{b}.{c}.{d} is not loopback and kevy has no AUTH/TLS yet.");
    eprintln!("kevy WARN: anyone who can reach this socket can read/write every key.");
    eprintln!("kevy WARN: safe only on trust-bounded networks (docker-compose internal,");
    eprintln!("kevy WARN: kubernetes pod network, VPC private subnet). Do NOT expose to");
    eprintln!("kevy WARN: the public internet. Front with stunnel/nginx + IP allowlist");
    eprintln!("kevy WARN: until AUTH/TLS lands in v0.3+.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_classification() {
        // 127.0.0.0/8 is loopback per RFC 1122 — every octet in [1..255] is fine.
        assert!(is_loopback([127, 0, 0, 1]));
        assert!(is_loopback([127, 255, 255, 254]));
        assert!(is_loopback([127, 1, 2, 3]));
        // Everything outside 127.* is reachable from some other host.
        assert!(!is_loopback([0, 0, 0, 0])); // wildcard — all interfaces
        assert!(!is_loopback([10, 0, 0, 1])); // RFC1918 private
        assert!(!is_loopback([192, 168, 1, 1])); // LAN
        assert!(!is_loopback([8, 8, 8, 8])); // public
    }

    #[test]
    fn ipv4_parser_accepts_valid_only() {
        assert_eq!(parse_ipv4("127.0.0.1"), Some([127, 0, 0, 1]));
        assert_eq!(parse_ipv4("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4("256.0.0.1"), None);
        assert_eq!(parse_ipv4("1.2.3"), None);
    }
}
