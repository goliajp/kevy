//! Operational commands required by valkey-compat clients but not tied
//! to keyspace state: `INFO`, `CLUSTER INFO / NODES`, `DEBUG SLEEP`,
//! `WAIT`, `SHUTDOWN`, `CONFIG`. All replies match the shape canonical
//! valkey clients (redis-rs, go-redis, jedis, etc.) expect at
//! handshake / housekeeping time.
//!
//! `CLIENT *` lives in a follow-up commit — it needs per-connection
//! state plumbed through the reactor → dispatch boundary.
//!
//! Subcommand-heavy verbs (currently `CONFIG`) live in submodules to
//! keep file size in line with the project's ≤ 500 LOC rule.

mod client;
mod config;

use std::time::SystemTime;

use kevy_config::Config;
use kevy_resp::{Argv, encode_bulk, encode_error, encode_integer, encode_simple_string};

use crate::config_global;

/// Operational-command dispatcher. Returns `true` if the verb was
/// recognised (and a reply has been written to `out`).
pub(crate) fn dispatch_ops(cmd: &[u8], args: &Argv, out: &mut Vec<u8>) -> bool {
    let cfg = config_global::get();
    match cmd {
        b"INFO" => cmd_info(&cfg, args, out),
        b"CLUSTER" => cmd_cluster(args, out),
        b"DEBUG" => cmd_debug(args, out),
        b"WAIT" => cmd_wait(args, out),
        b"SHUTDOWN" => cmd_shutdown(args, out),
        b"CONFIG" => config::cmd_config(&cfg, args, out),
        b"CLIENT" => client::cmd_client(args, out),
        _ => return false,
    }
    true
}

// ───────────── INFO ─────────────

fn cmd_info(cfg: &Config, args: &Argv, out: &mut Vec<u8>) {
    // INFO [section]; we always emit the requested section (or all when
    // none / "default" / "all" / "everything" is requested).
    let section = args.get(1).map(|a| a.to_ascii_lowercase());
    let want = section.as_deref();
    let mut body = String::new();
    if want_section(want, "server") {
        info_server(cfg, &mut body);
    }
    if want_section(want, "clients") {
        info_clients(&mut body);
    }
    if want_section(want, "memory") {
        info_memory(cfg, &mut body);
    }
    if want_section(want, "persistence") {
        info_persistence(cfg, &mut body);
    }
    if want_section(want, "stats") {
        info_stats(&mut body);
    }
    if want_section(want, "replication") {
        info_replication(&mut body);
    }
    if want_section(want, "cluster") {
        info_cluster(&mut body);
    }
    if want_section(want, "keyspace") {
        info_keyspace(&mut body);
    }
    encode_bulk(out, body.as_bytes());
}

fn want_section(want: Option<&[u8]>, name: &str) -> bool {
    match want {
        None => true,
        Some(s) if s == b"default" || s == b"all" || s == b"everything" => true,
        Some(s) => s == name.as_bytes(),
    }
}

fn info_server(cfg: &Config, b: &mut String) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    b.push_str("# Server\r\n");
    b.push_str("redis_version:7.4.0\r\n"); // valkey-compat byte-for-byte sniffing
    b.push_str(&format!("kevy_version:{}\r\n", env!("CARGO_PKG_VERSION")));
    b.push_str("redis_mode:standalone\r\n");
    b.push_str(&format!("process_id:{}\r\n", std::process::id()));
    b.push_str(&format!("tcp_port:{}\r\n", cfg.server.port));
    b.push_str(&format!("server_time_usec:{}\r\n", now * 1_000_000));
    b.push_str("\r\n");
}

fn info_clients(b: &mut String) {
    b.push_str("# Clients\r\n");
    b.push_str("connected_clients:1\r\n"); // TODO: real count when conn-info plumbed
    b.push_str("maxclients:10000\r\n");
    b.push_str("\r\n");
}

fn info_memory(cfg: &Config, b: &mut String) {
    b.push_str("# Memory\r\n");
    b.push_str("used_memory:0\r\n"); // TODO: real tracking lands in Wave 2 maxmemory
    b.push_str("used_memory_human:0B\r\n");
    b.push_str("used_memory_peak:0\r\n");
    b.push_str(&format!("maxmemory:{}\r\n", cfg.memory.maxmemory));
    b.push_str(&format!(
        "maxmemory_policy:{}\r\n",
        eviction_str(cfg.memory.maxmemory_policy)
    ));
    b.push_str("\r\n");
}

fn info_persistence(cfg: &Config, b: &mut String) {
    b.push_str("# Persistence\r\n");
    b.push_str("loading:0\r\n");
    b.push_str(&format!(
        "aof_enabled:{}\r\n",
        if cfg.persistence.aof { 1 } else { 0 }
    ));
    b.push_str(&format!(
        "appendfsync:{}\r\n",
        appendfsync_str(cfg.persistence.appendfsync)
    ));
    b.push_str("aof_rewrite_in_progress:0\r\n");
    b.push_str("aof_last_rewrite_time_sec:-1\r\n");
    b.push_str("\r\n");
}

fn info_stats(b: &mut String) {
    b.push_str("# Stats\r\n");
    b.push_str("total_connections_received:0\r\n");
    b.push_str("total_commands_processed:0\r\n");
    b.push_str("instantaneous_ops_per_sec:0\r\n");
    b.push_str("\r\n");
}

fn info_replication(b: &mut String) {
    b.push_str("# Replication\r\n");
    b.push_str("role:master\r\n");
    b.push_str("connected_slaves:0\r\n");
    b.push_str("master_replid:0000000000000000000000000000000000000000\r\n");
    b.push_str("master_repl_offset:0\r\n");
    b.push_str("\r\n");
}

fn info_cluster(b: &mut String) {
    b.push_str("# Cluster\r\n");
    b.push_str("cluster_enabled:0\r\n");
    b.push_str("\r\n");
}

fn info_keyspace(b: &mut String) {
    b.push_str("# Keyspace\r\n");
    // TODO: emit `db0:keys=N,expires=M,avg_ttl=...` when key-count is plumbed.
    b.push_str("\r\n");
}

// ───────────── CLUSTER ─────────────

fn cmd_cluster(args: &Argv, out: &mut Vec<u8>) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "cluster"),
    };
    match sub.as_slice() {
        b"INFO" => {
            // Same payload Redis returns for standalone (clients check
            // cluster_enabled:0 to skip CLUSTER SHARDS / CLUSTER SLOTS).
            let body = "cluster_enabled:0\r\n\
                        cluster_state:ok\r\n\
                        cluster_slots_assigned:16384\r\n\
                        cluster_slots_ok:16384\r\n\
                        cluster_slots_pfail:0\r\n\
                        cluster_slots_fail:0\r\n\
                        cluster_known_nodes:1\r\n\
                        cluster_size:1\r\n\
                        cluster_current_epoch:0\r\n\
                        cluster_my_epoch:0\r\n";
            encode_bulk(out, body.as_bytes());
        }
        b"NODES" => {
            // Single standalone node entry. Format documented at
            // https://redis.io/commands/cluster-nodes/. Most fields are
            // "-" when not applicable.
            let body = "0000000000000000000000000000000000000000 :0@0 myself,master - 0 0 0 connected 0-16383\r\n";
            encode_bulk(out, body.as_bytes());
        }
        b"MYID" => encode_bulk(
            out,
            b"0000000000000000000000000000000000000000",
        ),
        b"COUNTKEYSINSLOT" => encode_integer(out, 0),
        b"KEYSLOT" => encode_integer(out, 0),
        _ => encode_simple_string(out, "OK"),
    }
}

// ───────────── DEBUG ─────────────

fn cmd_debug(args: &Argv, out: &mut Vec<u8>) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "debug"),
    };
    match sub.as_slice() {
        b"SLEEP" => {
            let secs: f64 = args
                .get(2)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            if secs > 0.0 {
                let nanos = (secs * 1_000_000_000.0).clamp(0.0, u64::MAX as f64) as u64;
                std::thread::sleep(std::time::Duration::from_nanos(nanos));
            }
            encode_simple_string(out, "OK");
        }
        b"OBJECT" => encode_simple_string(out, "OK"),
        b"SET-ACTIVE-EXPIRE" => encode_simple_string(out, "OK"),
        _ => encode_simple_string(out, "OK"), // tolerant stub for any other DEBUG subcommand
    }
}

// ───────────── WAIT ─────────────

fn cmd_wait(args: &Argv, out: &mut Vec<u8>) {
    // WAIT numreplicas timeout — single-machine kevy has zero replicas,
    // so the answer "how many replicas acked your writes" is always 0.
    // Redis blocks until numreplicas or timeout; we return immediately.
    if args.len() != 3 {
        return wrong_args(out, "wait");
    }
    encode_integer(out, 0);
}

// ───────────── SHUTDOWN ─────────────

fn cmd_shutdown(args: &Argv, _out: &mut Vec<u8>) {
    // SHUTDOWN [NOSAVE | SAVE] — Redis exits without sending a reply
    // (client sees connection drop). v1.0 stub: parse the subcommand
    // for forward compatibility, then exit(0). Wave 2 will add the
    // AOF-flush-on-exit graceful path; for now we rely on appendfsync
    // = always or everysec to have flushed recent writes.
    let mode = args.get(1).map(|s| s.to_ascii_uppercase());
    let _ = mode; // accepted for parity; behavior identical for now
    std::process::exit(0);
}

// ───────────── value → string converters (shared with config submodule) ─────────────

pub(super) fn appendfsync_str(v: kevy_config::AppendFsync) -> &'static str {
    use kevy_config::AppendFsync::*;
    match v {
        Always => "always",
        EverySec => "everysec",
        No => "no",
    }
}

pub(super) fn eviction_str(v: kevy_config::EvictionPolicy) -> &'static str {
    use kevy_config::EvictionPolicy::*;
    match v {
        NoEviction => "noeviction",
        AllKeysLru => "allkeys-lru",
        AllKeysLfu => "allkeys-lfu",
        AllKeysRandom => "allkeys-random",
        VolatileLru => "volatile-lru",
        VolatileLfu => "volatile-lfu",
        VolatileRandom => "volatile-random",
        VolatileTtl => "volatile-ttl",
    }
}

pub(super) fn log_level_str(v: kevy_config::LogLevel) -> &'static str {
    use kevy_config::LogLevel::*;
    match v {
        Trace => "trace",
        Debug => "debug",
        Info => "info",
        Warn => "warning",
        Error => "error",
    }
}

// ───────────── helpers ─────────────

pub(super) fn wrong_args(out: &mut Vec<u8>, name: &str) {
    encode_error(
        out,
        &format!("ERR wrong number of arguments for '{name}' command"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(verb: &[u8], rest: &[&[u8]]) -> Vec<u8> {
        let mut a = Argv::default();
        a.push(verb);
        for r in rest {
            a.push(r);
        }
        let mut out = Vec::new();
        let handled = dispatch_ops(verb, &a, &mut out);
        assert!(handled, "verb {:?} not handled", String::from_utf8_lossy(verb));
        out
    }

    #[test]
    fn info_returns_bulk_with_sections() {
        let out = run(b"INFO", &[]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("$"), "INFO must reply as bulk string");
        assert!(s.contains("# Server"));
        assert!(s.contains("# Replication"));
        assert!(s.contains("role:master"));
        assert!(s.contains("cluster_enabled:0"));
    }

    #[test]
    fn info_specific_section() {
        let out = run(b"INFO", &[b"memory"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("# Memory"));
        assert!(!s.contains("# Server"));
    }

    #[test]
    fn cluster_info_carries_standalone_markers() {
        let out = run(b"CLUSTER", &[b"INFO"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("cluster_enabled:0"));
        assert!(s.contains("cluster_state:ok"));
    }

    #[test]
    fn cluster_nodes_single_self_entry() {
        let out = run(b"CLUSTER", &[b"NODES"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("myself,master"));
        assert!(s.contains("0-16383"));
    }

    #[test]
    fn debug_sleep_zero_returns_immediately() {
        let out = run(b"DEBUG", &[b"SLEEP", b"0"]);
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn debug_sleep_small_actually_sleeps() {
        let t = std::time::Instant::now();
        let out = run(b"DEBUG", &[b"SLEEP", b"0.05"]);
        let elapsed = t.elapsed();
        assert!(elapsed.as_millis() >= 40, "expected ≥ 40ms, got {elapsed:?}");
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn wait_returns_zero_replicas() {
        let out = run(b"WAIT", &[b"3", b"1000"]);
        assert_eq!(out, b":0\r\n");
    }

    #[test]
    fn wait_wrong_args_errors() {
        let out = run(b"WAIT", &[b"3"]);
        assert!(out.starts_with(b"-ERR"));
    }
}
