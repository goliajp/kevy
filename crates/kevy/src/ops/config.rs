//! `CONFIG GET / SET / REWRITE / RESETSTAT` subhandlers and the
//! `Config` → key-value flattener they need. Split out of
//! `super::mod` so the parent file stays under the project's 500-LOC cap.
//!
//! Wave 2 will turn `CONFIG SET` into a real hot-settable command for
//! the field whitelist documented in `crates/kevy-config/README.md`;
//! Wave 2 will also implement `CONFIG REWRITE` against the source
//! `kevy.toml` path. v1.0 ships these two as read-only errors with the
//! follow-up version called out in the message.

use kevy_config::Config;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_simple_string};

use super::{appendfsync_str, eviction_str, log_level_str, wrong_args};

pub(super) fn cmd_config<A: ArgvView + ?Sized>(cfg: &Config, args: &A, out: &mut Vec<u8>) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "config"),
    };
    match sub.as_slice() {
        b"GET" => cmd_config_get(cfg, args, out),
        b"SET" => cmd_config_set(args, out),
        b"REWRITE" => cmd_config_rewrite(out),
        b"RESETSTAT" => encode_simple_string(out, "OK"),
        _ => encode_error(
            out,
            &format!(
                "ERR unknown CONFIG subcommand '{}'",
                String::from_utf8_lossy(args.get(1).unwrap_or(&[][..]))
            ),
        ),
    }
}

fn cmd_config_get<A: ArgvView + ?Sized>(cfg: &Config, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "config|get");
    }
    // CONFIG GET pattern1 [pattern2 ...] — collect all (key, value) pairs
    // whose key matches any of the requested glob patterns. Reply is a
    // flat `[k1, v1, k2, v2, ...]` array (Redis convention).
    let mut hits: Vec<(&'static str, String)> = Vec::new();
    for i in 2..args.len() {
        let pat = args[i].to_ascii_lowercase();
        for (key, val) in config_pairs(cfg) {
            if glob_match(&pat, key.as_bytes()) && !hits.iter().any(|(k, _)| *k == key) {
                hits.push((key, val));
            }
        }
    }
    encode_array_len(out, (hits.len() * 2) as i64);
    for (k, v) in hits {
        encode_bulk(out, k.as_bytes());
        encode_bulk(out, v.as_bytes());
    }
}

fn cmd_config_set<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "config|set");
    }
    encode_error(
        out,
        "ERR CONFIG SET is read-only in kevy v1.0 — edit kevy.toml and \
         restart. Hot-settable in v1.x Wave 2 (maxmemory, maxmemory-policy, \
         appendfsync, hz, sample, log-level)",
    );
}

fn cmd_config_rewrite(out: &mut Vec<u8>) {
    encode_error(
        out,
        "ERR CONFIG REWRITE not yet supported — kevy v1.0's TOML file is \
         user-owned; lands alongside CONFIG SET in v1.x Wave 2",
    );
}

/// Flat list of redis-style `(key, value-as-string)` pairs the current
/// [`Config`] exposes via `CONFIG GET`. Keys use Redis convention
/// (lowercase, hyphenated) so the names match valkey docs verbatim.
fn config_pairs(cfg: &Config) -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = Vec::new();
    let [a, b, c, d] = cfg.server.bind;
    v.push(("bind", format!("{a}.{b}.{c}.{d}")));
    v.push(("port", cfg.server.port.to_string()));
    v.push(("io-threads", cfg.server.threads.to_string()));
    v.push(("dir", cfg.server.data_dir.display().to_string()));
    v.push(("appendonly", yes_no(cfg.persistence.aof)));
    v.push((
        "appendfsync",
        appendfsync_str(cfg.persistence.appendfsync).to_string(),
    ));
    v.push((
        "auto-aof-rewrite-percentage",
        cfg.persistence.auto_aof_rewrite_percentage.to_string(),
    ));
    v.push((
        "auto-aof-rewrite-min-size",
        cfg.persistence.auto_aof_rewrite_min_size.to_string(),
    ));
    v.push(("maxmemory", cfg.memory.maxmemory.to_string()));
    v.push((
        "maxmemory-policy",
        eviction_str(cfg.memory.maxmemory_policy).to_string(),
    ));
    v.push(("hz", cfg.expiry.hz.to_string()));
    v.push(("maxmemory-samples", cfg.expiry.sample.to_string()));
    v.push(("loglevel", log_level_str(cfg.log.level).to_string()));
    v
}

/// Minimal glob matcher — `*` matches any run of bytes; everything else
/// matches literally. Sufficient for CONFIG GET patterns
/// (`maxmemory*`, `*`, exact names). Doesn't handle `?` or `[...]` —
/// real Redis does but we've never seen a CONFIG GET use them in
/// production traffic.
fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    fn go(pat: &[u8], s: &[u8]) -> bool {
        match pat.split_first() {
            None => s.is_empty(),
            Some((&b'*', rest)) => {
                if rest.is_empty() {
                    return true;
                }
                (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            Some((&c, rest)) => match s.split_first() {
                Some((&first, srest)) if first == c => go(rest, srest),
                _ => false,
            },
        }
    }
    go(pat, s)
}

fn yes_no(b: bool) -> String {
    if b { "yes".into() } else { "no".into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn run(verb: &[u8], rest: &[&[u8]]) -> Vec<u8> {
        let mut a = Argv::default();
        a.push(verb);
        for r in rest {
            a.push(r);
        }
        let mut out = Vec::new();
        let cfg = Config::default();
        cmd_config(&cfg, &a, &mut out);
        let _ = verb;
        out
    }

    #[test]
    fn glob_matcher_handles_star_and_literal() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"max*", b"maxmemory"));
        assert!(glob_match(b"max*", b"maxmemory-policy"));
        assert!(glob_match(b"*memory*", b"maxmemory-policy"));
        assert!(glob_match(b"port", b"port"));
        assert!(!glob_match(b"port", b"ports"));
        assert!(!glob_match(b"max*", b"min-memory"));
        assert!(glob_match(b"", b""));
        assert!(!glob_match(b"", b"x"));
    }

    #[test]
    fn config_get_exact_key() {
        let out = run(b"CONFIG", &[b"GET", b"port"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("*2\r\n"), "got: {s:?}");
        assert!(s.contains("port"));
        assert!(s.contains("6004")); // default port
    }

    #[test]
    fn config_get_glob() {
        let out = run(b"CONFIG", &[b"GET", b"max*"]);
        let s = String::from_utf8(out).unwrap();
        // max* matches maxmemory + maxmemory-policy + maxmemory-samples
        // → 3 pairs → array of 6.
        assert!(s.starts_with("*6\r\n"), "got: {s:?}");
        assert!(s.contains("maxmemory"));
        assert!(s.contains("maxmemory-policy"));
    }

    #[test]
    fn config_get_unknown_key_returns_empty_array() {
        let out = run(b"CONFIG", &[b"GET", b"nonexistent-setting"]);
        assert_eq!(out, b"*0\r\n");
    }

    #[test]
    fn config_get_multiple_patterns() {
        let out = run(b"CONFIG", &[b"GET", b"port", b"bind"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("*4\r\n"));
        assert!(s.contains("port"));
        assert!(s.contains("bind"));
    }

    #[test]
    fn config_set_returns_useful_error() {
        let out = run(b"CONFIG", &[b"SET", b"maxmemory", b"2gb"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("-ERR"));
        assert!(s.contains("read-only in kevy v1.0"));
    }

    #[test]
    fn config_rewrite_returns_useful_error() {
        let out = run(b"CONFIG", &[b"REWRITE"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("-ERR"));
        assert!(s.contains("REWRITE"));
    }

    #[test]
    fn config_unknown_subcommand_errors() {
        let out = run(b"CONFIG", &[b"NUKE"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("-ERR"));
        assert!(s.contains("unknown CONFIG subcommand"));
    }
}
