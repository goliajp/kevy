//! kevy server entry point.
//!
//! Reads config in precedence order (top wins): CLI flags → env vars
//! → TOML file (auto-detected) → built-in defaults. See
//! [`kevy_config`] for the schema.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use kevy_config::{CliOverrides, Config};

/// Route every allocation in the process through `kevy-alloc`.
///
/// Behind a feature and off by default. An allocator has no run-time
/// switch — whatever it costs, it costs on every `SET`, `GET` and
/// published message — so the decision to build with it belongs to
/// whoever builds, and the measurement that justifies it is the
/// interleaved A/B in `bench/allocgate.sh` (M1, M2).
#[cfg(feature = "kevy-alloc")]
#[global_allocator]
static GLOBAL: kevy_alloc::KevyAlloc = kevy_alloc::KevyAlloc;

fn main() -> ! {
    handle_help_and_version();
    let mut cfg = resolve_config();
    let threads = resolve_thread_count(&mut cfg);
    validate_accept_shards(&cfg, threads);
    print_startup_banner(&cfg, threads);
    if !is_loopback(cfg.server.bind) {
        warn_unprotected_bind(cfg.server.bind);
    }
    kevy::serve(Arc::new(cfg)); // never returns
}

/// `--help` / `--version` short-circuit BEFORE the config layer
/// touches anything, so they work even when the environment or
/// TOML is misconfigured. The standard CLI contract is "`--help`
/// is always reachable", which Docker healthchecks in particular
/// depend on.
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

/// `--accept-shards` runtime validation. `None` (default) = every
/// shard arms accept. `Some(N)` requires `1 <= N <= threads`.
fn validate_accept_shards(cfg: &Config, threads: usize) {
    let Some(n) = cfg.server.accept_shards else { return };
    if n == 0 || n > threads {
        eprintln!("kevy: --accept-shards must be in 1..={threads}, got {n} (threads = {threads})");
        std::process::exit(2);
    }
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
    --tiering-budget <B> Enable transparent tiering with this RAM budget:
                        \"auto\" (0.70 x detected memory bound), \"70%\"
                        (percent of the bound), or absolute (\"4gb\")
    --cluster           Single-node cluster mode: slot routing + one extra
                        deterministic port per shard (port+1+i); cluster
                        clients (redis-cli -c, redis-benchmark --cluster)
                        address shards directly, others use the main port
    -h, --help          Show this help and exit
    -V, --version       Print version and exit

Precedence (top wins): CLI flags > env vars > TOML file > built-in defaults.
Env vars: KEVY_BIND, KEVY_PORT, KEVY_THREADS, KEVY_DIR, KEVY_AOF, KEVY_CLUSTER,
KEVY_TIER_BUDGET (auto | N% | bytes/size literal).

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
///
/// Strict, like the env and TOML layers: an unknown flag, a flag given
/// twice, a missing value or a value that does not parse exits 2 naming
/// it. (`--port abc` used to be dropped, and the server listened on 6004.)
fn parse_cli() -> (Option<PathBuf>, CliOverrides) {
    parse_args(std::env::args().skip(1)).unwrap_or_else(|msg| {
        eprintln!("kevy: {msg} (kevy --help lists the flags)");
        std::process::exit(2);
    })
}

/// The flags, from `args` (without the program name).
fn parse_args(
    args: impl IntoIterator<Item = String>,
) -> Result<(Option<PathBuf>, CliOverrides), String> {
    let mut config = None;
    let mut o = CliOverrides::default();
    let mut seen: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        if matches!(flag.as_str(), "--help" | "-h" | "--version" | "-V") {
            continue; // answered before config is read
        }
        if seen.contains(&flag) {
            return Err(format!("{flag} given twice"));
        }
        seen.push(flag.clone());
        if matches!(flag.as_str(), "--no-aof" | "--cluster") {
            if inline.is_some() {
                return Err(format!("{flag} takes no value"));
            }
            if flag == "--no-aof" {
                o.aof = Some(false)
            } else {
                o.cluster = Some(true)
            }
            continue;
        }
        const VALUED: &[&str] = &[
            "--config",
            "--dir",
            "--bind",
            "--port",
            "--threads",
            "--accept-shards",
            "--tiering-budget",
        ];
        if !VALUED.contains(&flag.as_str()) {
            return Err(format!("unknown flag '{flag}'"));
        }
        let value = inline.or_else(|| args.next()).ok_or(format!("{flag} needs a value"))?;
        apply_flag(&flag, value, &mut config, &mut o)?;
    }
    Ok((config, o))
}

/// One flag that takes a value.
fn apply_flag(
    flag: &str,
    value: String,
    config: &mut Option<PathBuf>,
    o: &mut CliOverrides,
) -> Result<(), String> {
    let bad = |what: &str| format!("{flag} takes {what}, not '{value}'");
    match flag {
        "--config" => *config = Some(PathBuf::from(&value)),
        "--dir" => o.data_dir = Some(PathBuf::from(&value)),
        "--bind" => o.bind = Some(parse_ipv4(&value).ok_or_else(|| bad("an IPv4 address"))?),
        "--port" => o.port = Some(value.parse().map_err(|_| bad("a port (0-65535)"))?),
        "--threads" => o.threads = Some(value.parse().map_err(|_| bad("a shard count"))?),
        "--accept-shards" => {
            o.accept_shards = Some(value.parse().map_err(|_| bad("a shard count"))?)
        }
        "--tiering-budget" => {
            o.tiering_budget = Some(
                kevy_config::TierBudgetSpec::parse(&value).map_err(|e| format!("{flag}: {e}"))?,
            )
        }
        other => return Err(format!("unknown flag '{other}'")),
    }
    Ok(())
}

/// Snapshot the process env as `(String, String)` pairs for
/// `Config::merge_env`. We materialize because `Config::merge_env`
/// expects an owned iterator (so tests can fixture an in-memory map
/// without touching the global env).
fn env_vars() -> impl IntoIterator<Item = (String, String)> {
    std::env::vars().collect::<Vec<_>>()
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

/// Valkey/Redis "protected-mode" style advisory. kevy has no auth
/// (a deliberate non-goal); the only safe deployment for a non-loopback
/// bind is a trust-bounded network (docker-compose internal, kubernetes
/// pod network, VPC private subnet). For public exposure, front with
/// stunnel/nginx + IP allowlist.
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

    fn parsed(line: &str) -> Result<(Option<PathBuf>, CliOverrides), String> {
        parse_args(line.split_whitespace().map(String::from))
    }

    #[test]
    fn server_flags_are_parsed_or_refused_by_name() {
        let (config, o) =
            parsed("--port 6380 --bind=0.0.0.0 --threads 0 --no-aof --config k.toml").unwrap();
        assert_eq!(
            (o.port, o.bind, o.threads, o.aof),
            (Some(6380), Some([0, 0, 0, 0]), Some(0), Some(false))
        );
        assert_eq!(config, Some(PathBuf::from("k.toml")));
        for (line, msg) in [
            ("--port abc", "--port takes a port (0-65535), not 'abc'"),
            ("--port 70000", "--port takes a port (0-65535), not '70000'"),
            ("--bind 1.2.3", "--bind takes an IPv4 address, not '1.2.3'"),
            ("--threads -1", "--threads takes a shard count, not '-1'"),
            ("--port", "--port needs a value"),
            ("--port 1 --port 2", "--port given twice"),
            ("--no-aof=yes", "--no-aof takes no value"),
            ("--save 60", "unknown flag '--save'"),
            ("6004", "unknown flag '6004'"),
        ] {
            assert_eq!(parsed(line).err().as_deref(), Some(msg), "{line}");
        }
        assert!(parsed("--help --port 1").is_ok(), "help is answered earlier");
    }

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
