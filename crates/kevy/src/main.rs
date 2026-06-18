//! kevy server entry point.
//!
//! Reads config in precedence order (top wins): CLI flags → env vars
//! → TOML file (auto-detected) → built-in defaults. See
//! [`kevy_config`] for the schema.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use kevy_config::{CliOverrides, Config};

fn main() -> ! {
    handle_help_and_version();
    let mut cfg = resolve_config();
    let threads = resolve_thread_count(&mut cfg);
    print_startup_banner(&cfg, threads);
    if !is_loopback(cfg.server.bind) {
        warn_unprotected_bind(cfg.server.bind);
    }
    let bind = cfg.server.bind;
    let port = cfg.server.port;
    let data_dir = cfg.server.data_dir.clone();
    let aof = cfg.persistence.aof;
    // Install the resolved Config process-wide so dispatch handlers
    // (INFO, CONFIG GET) read live values instead of compile-time
    // defaults. Must happen before the reactor starts so shards see it.
    kevy::config_init(Arc::new(cfg));
    kevy::serve(bind, port, threads, data_dir, aof); // never returns
}

/// `--help` / `--version` short-circuit BEFORE we touch the config layer, so
/// they work even if the env / TOML is misconfigured (the standard CLI
/// contract: --help always reachable). Spotted in v1.0.x downstream usage
/// — mailrs's Docker healthcheck silently no-op'd because `kevy --help` was
/// ignored, leading to a server-process being treated as a CLI tool.
fn handle_help_and_version() {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("kevy {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

fn resolve_config() -> Config {
    let (config_path, cli) = parse_cli();
    let mut cfg = Config::load(config_path.as_deref()).unwrap_or_else(die);
    cfg.merge_env(env_vars()).unwrap_or_else(die);
    cfg.merge_cli(cli).unwrap_or_else(die);
    cfg
}

fn die<E: std::fmt::Display, T>(e: E) -> T {
    eprintln!("{e}");
    std::process::exit(1);
}

/// Resolve `threads = 0 (auto)` into the actual count and write it back so
/// CLUSTER SLOTS / SHARDS / NODES (which read the process-wide config) see
/// the real shard count, not the `0 = auto` sentinel.
fn resolve_thread_count(cfg: &mut Config) -> usize {
    let threads = if cfg.server.threads == 0 {
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
    } else {
        cfg.server.threads
    };
    cfg.server.threads = threads;
    threads
}

fn print_startup_banner(cfg: &Config, threads: usize) {
    let [a, b, c, d] = cfg.server.bind;
    eprintln!(
        "kevy v{} starting: {a}.{b}.{c}.{d}:{}, {threads} shard(s), dir={}, aof={}{} (thread-per-core)",
        env!("CARGO_PKG_VERSION"),
        cfg.server.port,
        cfg.server.data_dir.display(),
        if cfg.persistence.aof { "on" } else { "off" },
        if cfg.cluster.enabled { ", cluster" } else { "" },
    );
}

fn print_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!(
        "\
kevy {v} — pure-Rust Redis-compatible KV server.

USAGE:
    kevy [OPTIONS]

OPTIONS:
    --config <PATH>     TOML config file (auto-detected: ./kevy.toml,
                        /etc/kevy/kevy.toml, $XDG_CONFIG_HOME/kevy/kevy.toml)
    --bind <IPv4>       Bind address (default: 127.0.0.1)
    --port <PORT>       Listen port (default: 6004)
    --threads <N>       Shard count (default: 0 = available_parallelism())
    --dir <PATH>        Data directory for snapshot + AOF (default: .)
    --no-aof            Disable the AOF (in-memory only / cache-only mode)
    --cluster           Single-node cluster mode: slot routing + one extra
                        deterministic port per shard (port+1+i); cluster
                        clients (redis-cli -c, redis-benchmark --cluster)
                        address shards directly, others use the main port
    -h, --help          Show this help and exit
    -V, --version       Print version and exit

Precedence (top wins): CLI flags > env vars > TOML file > built-in defaults.
Env vars: KEVY_BIND, KEVY_PORT, KEVY_THREADS, KEVY_DIR, KEVY_AOF, KEVY_CLUSTER.

EXAMPLES:
    kevy                        # 127.0.0.1:6004, all cores, AOF on
    kevy --bind 0.0.0.0 --port 6379
    kevy --config /etc/kevy/kevy.toml
    KEVY_BIND=0.0.0.0 KEVY_AOF=0 kevy

CLI client for healthchecks / one-shot commands: see `kevy-cli --help`.

Docs: https://github.com/goliajp/kevy"
    );
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
    let cluster = if std::env::args().any(|a| a == "--cluster") {
        Some(true)
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
        cluster,
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
