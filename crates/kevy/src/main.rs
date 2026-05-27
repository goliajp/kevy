//! kevy server entry point.
#![forbid(unsafe_code)]

const DEFAULT_PORT: u16 = 6004;
const DEFAULT_BIND: [u8; 4] = [127, 0, 0, 1];

fn main() -> ! {
    let port = resolve_port();
    let bind = resolve_bind();
    let shards = resolve_shards();
    let dir = resolve_data_dir();
    let aof = resolve_aof();
    let [a, b, c, d] = bind;
    eprintln!(
        "kevy v{} starting: {a}.{b}.{c}.{d}:{port}, {shards} shard(s), dir={}, aof={} (thread-per-core)",
        env!("CARGO_PKG_VERSION"),
        dir.display(),
        if aof { "on" } else { "off" }
    );
    if !is_loopback(bind) {
        warn_unprotected_bind(bind);
    }
    kevy::serve(bind, port, shards, dir, aof); // never returns
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

/// AOF enabled unless `--no-aof` / `KEVY_AOF=0|off|false`.
fn resolve_aof() -> bool {
    if std::env::args().any(|a| a == "--no-aof") {
        return false;
    }
    !matches!(
        std::env::var("KEVY_AOF").ok().as_deref(),
        Some("0") | Some("off") | Some("false") | Some("no")
    )
}

/// Port precedence: `--port N` arg, then `KEVY_PORT` env, then the default.
fn resolve_port() -> u16 {
    arg_value("--port")
        .and_then(|s| s.parse().ok())
        .or_else(|| std::env::var("KEVY_PORT").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(DEFAULT_PORT)
}

/// Bind address precedence: `--bind A.B.C.D`, then `KEVY_BIND`, then loopback.
fn resolve_bind() -> [u8; 4] {
    arg_value("--bind")
        .and_then(|s| parse_ipv4(&s))
        .or_else(|| std::env::var("KEVY_BIND").ok().and_then(|s| parse_ipv4(&s)))
        .unwrap_or(DEFAULT_BIND)
}

/// Shard/thread count: `--threads N`, then `KEVY_THREADS`, then CPU count.
fn resolve_shards() -> usize {
    arg_value("--threads")
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            std::env::var("KEVY_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Data directory for snapshots: `--dir PATH`, then `KEVY_DIR`, then `.`.
fn resolve_data_dir() -> std::path::PathBuf {
    arg_value("--dir")
        .or_else(|| std::env::var("KEVY_DIR").ok())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
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
}
