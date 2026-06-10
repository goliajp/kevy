//! `CONFIG GET / SET / REWRITE / RESETSTAT` subhandlers and the
//! `Config` → key-value flattener they need. Split out of
//! `super::mod` so the parent file stays under the project's 500-LOC cap.
//!
//! `CONFIG SET` and `CONFIG REWRITE` are now real:
//! - SET validates against the **hot-settable matrix** locked in
//!   `V1.0-BOUNDARY.md`. Hot-settable knobs (`maxmemory`,
//!   `maxmemory-policy`, `appendfsync`, `auto-aof-rewrite-*`, `hz`,
//!   `maxmemory-samples`, `loglevel`, `logfile`-stdout/stderr) build
//!   a fresh `Arc<Config>` and atomically swap [`crate::config_global`].
//!   Per-shard re-application happens lazily on the next tick via
//!   `kevy_rt::Commands::live_runtime_config` (~100 ms upper bound on
//!   propagation; well under Redis's "best-effort" semantics).
//! - Non-hot-settable fields (`bind`, `port`, `threads`, `dir`,
//!   `appendonly`, `logfile`-with-path) return Redis's canonical
//!   `ERR ... can't be changed at runtime` form.
//! - REWRITE re-emits the live config via `Config::to_toml_string`
//!   and rename-overwrites the source file atomically. Per the v1.0
//!   matrix, inline comments are NOT preserved; the reply notes this.

use std::path::PathBuf;
use std::sync::Arc;

use kevy_config::{AppendFsync, Config, EvictionPolicy, LogLevel, LogOutput, parse_size};
use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_map_header,
    encode_simple_string,
};

use crate::config_global;

use super::{appendfsync_str, eviction_str, log_level_str, wrong_args};

pub(crate) fn cmd_config<A: ArgvView + ?Sized>(
    cfg: &Config,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "config"),
    };
    match sub.as_slice() {
        b"GET" => cmd_config_get(cfg, args, out, proto),
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

fn cmd_config_get<A: ArgvView + ?Sized>(
    cfg: &Config,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 3 {
        return wrong_args(out, "config|get");
    }
    // CONFIG GET pattern1 [pattern2 ...] — collect all (key, value) pairs
    // whose key matches any of the requested glob patterns. Reply shape:
    // V2 — flat `*2N\r\n[k1, v1, k2, v2, ...]` array (Redis legacy).
    // V3 — `%N\r\n[k1, v1, k2, v2, ...]` Map (per the RESP3 spec — kv
    //      replies are real maps).
    let mut hits: Vec<(&'static str, String)> = Vec::new();
    for i in 2..args.len() {
        let pat = args[i].to_ascii_lowercase();
        for (key, val) in config_pairs(cfg) {
            if glob_match(&pat, key.as_bytes()) && !hits.iter().any(|(k, _)| *k == key) {
                hits.push((key, val));
            }
        }
    }
    match proto {
        RespVersion::V2 => encode_array_len(out, (hits.len() * 2) as i64),
        RespVersion::V3 => encode_map_header(out, hits.len() as i64),
    }
    for (k, v) in hits {
        encode_bulk(out, k.as_bytes());
        encode_bulk(out, v.as_bytes());
    }
}

fn cmd_config_set<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "config|set");
    }
    let key = args[2].to_ascii_lowercase();
    let value = &args[3];
    let live = config_global::get();
    let mut new_cfg = (*live).clone();
    match apply_hot_set(&mut new_cfg, &key, value) {
        Ok(()) => match config_global::replace(Arc::new(new_cfg)) {
            Ok(()) => encode_simple_string(out, "OK"),
            Err(e) => encode_error(out, &format!("ERR {e}")),
        },
        Err(SetError::ReadOnly(k)) => encode_error(
            out,
            &format!("ERR config setting '{k}' can't be changed at runtime, restart required"),
        ),
        Err(SetError::Unknown(k)) => encode_error(
            out,
            &format!("ERR Unknown CONFIG SET parameter: '{k}'"),
        ),
        Err(SetError::BadValue { key, reason }) => encode_error(
            out,
            &format!("ERR CONFIG SET failed for '{key}': {reason}"),
        ),
    }
}

fn cmd_config_rewrite(out: &mut Vec<u8>) {
    let cfg = config_global::get();
    let Some(path) = cfg.source_path.clone() else {
        return encode_error(
            out,
            "ERR The server is running without a config file",
        );
    };
    let text = rewrite_text(&cfg, &path);
    match atomic_write(&path, text.as_bytes()) {
        Ok(()) => encode_simple_string(out, "OK"),
        Err(e) => encode_error(
            out,
            &format!(
                "ERR CONFIG REWRITE could not write {}: {e}",
                path.display()
            ),
        ),
    }
}

/// Build the rewrite payload, preferring the comment-preserving path
/// (re-parse original source line-by-line, splice values in place) and
/// falling back to [`Config::to_toml_string`] when the source can't be
/// read or re-parsed.
fn rewrite_text(cfg: &Config, path: &PathBuf) -> String {
    match std::fs::read_to_string(path) {
        Ok(src) => match cfg.to_toml_string_preserving(&src) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "kevy: CONFIG REWRITE comment-preserving re-parse of {} \
                     failed ({e}); falling back to standard template (comments lost)",
                    path.display(),
                );
                cfg.to_toml_string()
            }
        },
        Err(e) => {
            eprintln!(
                "kevy: CONFIG REWRITE could not read {} for comment preservation \
                 ({e}); falling back to standard template (comments lost)",
                path.display(),
            );
            cfg.to_toml_string()
        }
    }
}

/// Atomic-rename file write: dump to `<path>.rewrite` with fsync,
/// then `rename(2)` over the live path. Tolerates the temp-file
/// existing from a prior crashed rewrite (overwritten on next try).
fn atomic_write(path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.clone();
    let new_name = match path.file_name() {
        Some(n) => {
            let mut s = n.to_os_string();
            s.push(".rewrite");
            s
        }
        None => return Err(std::io::Error::other("CONFIG REWRITE path has no file name")),
    };
    tmp.set_file_name(new_name);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_data()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug)]
enum SetError {
    /// Field exists but the v1.0 hot-settable matrix marks it as
    /// requiring a restart (bind, port, threads, dir, appendonly,
    /// logfile-with-path).
    ReadOnly(String),
    /// Field name is not recognised by the schema at all.
    Unknown(String),
    /// Field exists + is hot-settable, but the value didn't parse.
    BadValue { key: String, reason: String },
}

/// Hot-set one field on `cfg` based on its Redis-style key. Returns
/// `Ok(())` on a clean apply or an [`SetError`] describing the refusal.
fn apply_hot_set(cfg: &mut Config, key: &[u8], value: &[u8]) -> Result<(), SetError> {
    let key_str = std::str::from_utf8(key)
        .map_err(|_| SetError::Unknown(String::from_utf8_lossy(key).into_owned()))?;
    let value_str = std::str::from_utf8(value)
        .map_err(|_| SetError::BadValue {
            key: key_str.to_string(),
            reason: "value is not valid UTF-8".to_string(),
        })?;
    match key_str {
        "maxmemory" | "maxmemory-policy" => set_memory(cfg, key_str, value_str),
        "appendfsync" | "auto-aof-rewrite-percentage" | "auto-aof-rewrite-min-size" => {
            set_persistence(cfg, key_str, value_str)
        }
        "hz" | "maxmemory-samples" => set_expiry(cfg, key_str, value_str),
        "loglevel" | "logfile" => set_log(cfg, key_str, value_str),
        "bind" | "port" | "io-threads" | "dir" | "appendonly" => {
            Err(SetError::ReadOnly(key_str.to_string()))
        }
        other => Err(SetError::Unknown(other.to_string())),
    }
}

fn set_memory(cfg: &mut Config, key: &str, value: &str) -> Result<(), SetError> {
    match key {
        "maxmemory" => {
            cfg.memory.maxmemory = parse_size(value).map_err(|reason| SetError::BadValue {
                key: key.to_string(),
                reason,
            })?;
        }
        "maxmemory-policy" => {
            cfg.memory.maxmemory_policy = EvictionPolicy::parse(value).ok_or_else(|| {
                SetError::BadValue {
                    key: key.to_string(),
                    reason: "expected one of noeviction / allkeys-lru / \
                             allkeys-lfu / allkeys-random / volatile-lru / \
                             volatile-lfu / volatile-random / volatile-ttl"
                        .to_string(),
                }
            })?;
        }
        _ => return Err(SetError::Unknown(key.to_string())),
    }
    Ok(())
}

fn set_persistence(cfg: &mut Config, key: &str, value: &str) -> Result<(), SetError> {
    match key {
        "appendfsync" => {
            cfg.persistence.appendfsync =
                AppendFsync::parse(value).ok_or_else(|| SetError::BadValue {
                    key: key.to_string(),
                    reason: "expected one of always / everysec / no".to_string(),
                })?;
        }
        "auto-aof-rewrite-percentage" => {
            cfg.persistence.auto_aof_rewrite_percentage =
                value.parse::<u32>().map_err(|_| SetError::BadValue {
                    key: key.to_string(),
                    reason: "expected a non-negative integer".to_string(),
                })?;
        }
        "auto-aof-rewrite-min-size" => {
            cfg.persistence.auto_aof_rewrite_min_size =
                parse_size(value).map_err(|reason| SetError::BadValue {
                    key: key.to_string(),
                    reason,
                })?;
        }
        _ => return Err(SetError::Unknown(key.to_string())),
    }
    Ok(())
}

fn set_expiry(cfg: &mut Config, key: &str, value: &str) -> Result<(), SetError> {
    let n = value.parse::<u32>().map_err(|_| SetError::BadValue {
        key: key.to_string(),
        reason: "expected a non-negative integer".to_string(),
    })?;
    match key {
        "hz" => cfg.expiry.hz = n,
        "maxmemory-samples" => cfg.expiry.sample = n,
        _ => return Err(SetError::Unknown(key.to_string())),
    }
    Ok(())
}

fn set_log(cfg: &mut Config, key: &str, value: &str) -> Result<(), SetError> {
    match key {
        "loglevel" => {
            cfg.log.level = LogLevel::parse(value).ok_or_else(|| SetError::BadValue {
                key: key.to_string(),
                reason: "expected one of trace / debug / info / warning / error"
                    .to_string(),
            })?;
        }
        "logfile" => {
            // Redis names this `logfile`; kevy's TOML calls it `log.output`.
            // Per the v1.0 hot-settable matrix, only stdout / stderr are
            // hot-settable. Any file path requires opening a handle the
            // shards can write to — punted to the v1.x log-layer rewrite.
            match LogOutput::parse(value) {
                LogOutput::Stdout => cfg.log.output = LogOutput::Stdout,
                LogOutput::Stderr => cfg.log.output = LogOutput::Stderr,
                LogOutput::File(_) => return Err(SetError::ReadOnly(key.to_string())),
            }
        }
        _ => return Err(SetError::Unknown(key.to_string())),
    }
    Ok(())
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
    v.push(("cluster-enabled", yes_no(cfg.cluster.enabled)));
    v.push((
        "cluster-port-base",
        crate::cluster_port_base(cfg).to_string(),
    ));
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
#[path = "config_tests.rs"]
mod tests;
