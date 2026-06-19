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

// INFO emits ~20 lines per call, called once per session handshake — the
// `push_str(&format!(...))` shape is the legible per-line pattern; `write!`
// adds `let _ =` boilerplate without measurable savings (INFO is not on the
// command hot path).
#![allow(clippy::format_push_string)]

pub(crate) mod client;
pub(crate) mod cluster;
pub(crate) mod config;
mod memory;
pub(crate) mod replication;
pub(crate) mod scope_move;
pub(crate) mod stats;

use std::time::SystemTime;

use kevy_config::Config;
use kevy_resp::{
    ArgvView, RespVersion, encode_bulk, encode_error, encode_integer, encode_simple_string,
    encode_verbatim,
};
use kevy_store::Store;

use crate::config_global;

/// Operational-command dispatcher. Returns `true` if the verb was
/// recognised (and a reply has been written to `out`). `config_global::get`
/// is paid only inside the arms that actually need it — GET / SET and the
/// other string / collection verbs flow past via the early `_ => false`
/// without touching the global config Arc clone.
pub(crate) fn dispatch_ops<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"INFO" => {
            let cfg = config_global::get();
            cmd_info(&cfg, store, args, out, RespVersion::V2);
        }
        b"CLUSTER" => {
            let cfg = config_global::get();
            cluster::cmd_cluster(&cfg, store, args, out);
        }
        b"DEBUG" => cmd_debug(args, out),
        b"WAIT" => cmd_wait(args, out),
        b"SHUTDOWN" => cmd_shutdown(args, out),
        b"CONFIG" => {
            let cfg = config_global::get();
            config::cmd_config(&cfg, args, out, RespVersion::V2);
        }
        b"CLIENT" => client::cmd_client(args, out, RespVersion::V2),
        b"ROLE" => replication::cmd_role(args, out),
        b"REPLICAOF" | b"SLAVEOF" => replication::cmd_replicaof(args, out),
        b"MOVE-SCOPE" => scope_move::cmd_move_scope(store, args, out),
        b"MOVE-SCOPE-INGEST" => scope_move::cmd_move_scope_ingest(store, args, out),
        b"MEMORY" => {
            let cfg = config_global::get();
            memory::cmd_memory(&cfg, store, args, out);
        }
        _ => return false,
    }
    true
}

// ───────────── INFO ─────────────

pub(crate) fn cmd_info<A: ArgvView + ?Sized>(
    cfg: &Config,
    store: &Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    // INFO [section]; we always emit the requested section (or all when
    // none / "default" / "all" / "everything" is requested).
    let section = args.get(1).map(<[u8]>::to_ascii_lowercase);
    let want = section.as_deref();
    // Each shard owns an independent store; INFO is answered on one shard but
    // reports the whole process. Freshen this shard's slot from the live store
    // it already holds (so the answering shard is never stale, even with the
    // active reaper disabled), then sum every shard's slot.
    stats::publish_gauges(store);
    let totals = stats::aggregate();
    let mut body = String::new();
    if want_section(want, "server") {
        info_server(cfg, &mut body);
    }
    if want_section(want, "clients") {
        info_clients(&mut body);
    }
    if want_section(want, "memory") {
        info_memory(cfg, &totals, &mut body);
    }
    if want_section(want, "persistence") {
        info_persistence(cfg, &mut body);
    }
    if want_section(want, "stats") {
        info_stats(&totals, &mut body);
    }
    if want_section(want, "replication") {
        info_replication(&mut body);
    }
    if want_section(want, "cluster") {
        info_cluster(cfg, &mut body);
    }
    if want_section(want, "keyspace") {
        info_keyspace(&totals, &mut body);
    }
    // RESP3: Verbatim text frame (`=N\r\ntxt:<body>\r\n`) so the
    // client can render it as plain text (e.g. redis-cli prints it
    // unchanged). RESP2 stays as a length-prefixed bulk.
    match proto {
        RespVersion::V2 => encode_bulk(out, body.as_bytes()),
        RespVersion::V3 => encode_verbatim(out, *b"txt", body.as_bytes()),
    }
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
        .map_or(0, |d| d.as_secs());
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

fn info_memory(cfg: &Config, totals: &stats::Totals, b: &mut String) {
    let used = totals.used_memory;
    let peak = totals.used_memory_peak;
    b.push_str("# Memory\r\n");
    b.push_str(&format!("used_memory:{used}\r\n"));
    b.push_str(&format!(
        "used_memory_human:{}\r\n",
        memory::format_bytes_human(used)
    ));
    b.push_str(&format!("used_memory_peak:{peak}\r\n"));
    b.push_str(&format!(
        "used_memory_peak_human:{}\r\n",
        memory::format_bytes_human(peak)
    ));
    b.push_str(&format!("maxmemory:{}\r\n", cfg.memory.maxmemory));
    b.push_str(&format!(
        "maxmemory_human:{}\r\n",
        memory::format_bytes_human(cfg.memory.maxmemory)
    ));
    b.push_str(&format!(
        "maxmemory_policy:{}\r\n",
        eviction_str(cfg.memory.maxmemory_policy)
    ));
    b.push_str(&format!("evicted_keys:{}\r\n", totals.evicted_keys));
    b.push_str("\r\n");
}

thread_local! {
    /// The answering shard's background-persistence view, refreshed by the
    /// reactor tick via `Commands::on_persist_stats` (thread-per-core:
    /// thread == shard, the `cluster::CURRENT_SHARD` pattern). Stale by at
    /// most one tick interval. `(in_flight, aof_rewrites_total)`.
    static PERSIST_STATS: std::cell::Cell<(bool, u64)> =
        const { std::cell::Cell::new((false, 0)) };
}

/// Record the reactor's persistence stats for `INFO persistence` (see
/// [`PERSIST_STATS`]).
pub(crate) fn set_persist_stats(in_flight: bool, aof_rewrites_total: u64) {
    PERSIST_STATS.with(|c| c.set((in_flight, aof_rewrites_total)));
}

fn info_persistence(cfg: &Config, b: &mut String) {
    let (in_flight, rewrites) = PERSIST_STATS.with(std::cell::Cell::get);
    b.push_str("# Persistence\r\n");
    b.push_str("loading:0\r\n");
    b.push_str(&format!(
        "aof_enabled:{}\r\n",
        i32::from(cfg.persistence.aof)
    ));
    b.push_str(&format!(
        "appendfsync:{}\r\n",
        appendfsync_str(cfg.persistence.appendfsync)
    ));
    // The answering shard's view (each shard persists independently);
    // refreshed per reactor tick, so in-progress flips within ~100 ms of
    // a BGSAVE/BGREWRITEAOF starting or finishing.
    b.push_str(&format!(
        "aof_rewrite_in_progress:{}\r\n",
        i32::from(in_flight)
    ));
    b.push_str(&format!("aof_rewrites_total:{rewrites}\r\n"));
    b.push_str("aof_last_rewrite_time_sec:-1\r\n");
    b.push_str("\r\n");
}

fn info_stats(totals: &stats::Totals, b: &mut String) {
    b.push_str("# Stats\r\n");
    b.push_str(&format!(
        "total_connections_received:{}\r\n",
        totals.connections_received
    ));
    b.push_str(&format!(
        "total_commands_processed:{}\r\n",
        totals.commands_processed
    ));
    b.push_str(&format!(
        "instantaneous_ops_per_sec:{}\r\n",
        stats::instantaneous_ops_per_sec(totals.commands_processed)
    ));
    b.push_str(&format!("expired_keys:{}\r\n", totals.expired_keys));
    b.push_str("\r\n");
}

fn info_replication(b: &mut String) {
    // T1.31: live `INFO replication` — reads `current_upstream()` to
    // decide the section shape, then drains the per-tick view
    // (`replication_view()`) for offset + connected-replicas count.
    // The fields mirror Redis 7.x; the v1.18 simplifications are:
    //   - master_replid is a single zeros-string (no failover ID
    //     bookkeeping yet — kevy-elect (Phase 1.5) introduces real IDs)
    //   - master_link_status is fixed to "up" when an upstream is
    //     installed (no runner→view feedback yet — T1.31.x follow-up)
    //   - the per-replica list is omitted (peer-addr capture is
    //     T1.28.5 — see plan).
    b.push_str("# Replication\r\n");
    let upstream = crate::replica_state::current_upstream();
    let view = replication::replication_view();
    let offset = view.master_repl_offset;
    let connected = view.replicas.len();
    match upstream {
        Some((host, port)) => {
            b.push_str("role:slave\r\n");
            b.push_str(&format!("master_host:{host}\r\n"));
            b.push_str(&format!("master_port:{port}\r\n"));
            b.push_str("master_link_status:up\r\n");
            b.push_str("master_sync_in_progress:0\r\n");
            b.push_str("slave_read_only:0\r\n");
            b.push_str("slave_repl_offset:0\r\n");
        }
        None => {
            b.push_str("role:master\r\n");
            b.push_str(&format!("connected_slaves:{connected}\r\n"));
            b.push_str("master_replid:0000000000000000000000000000000000000000\r\n");
            b.push_str(&format!("master_repl_offset:{offset}\r\n"));
        }
    }
    b.push_str("\r\n");
}

fn info_cluster(cfg: &Config, b: &mut String) {
    b.push_str("# Cluster\r\n");
    b.push_str(if cfg.cluster.enabled {
        "cluster_enabled:1\r\n"
    } else {
        "cluster_enabled:0\r\n"
    });
    b.push_str("\r\n");
}

fn info_keyspace(totals: &stats::Totals, b: &mut String) {
    b.push_str("# Keyspace\r\n");
    // Redis omits the `dbN:` line entirely for an empty keyspace. `avg_ttl` is
    // a Redis estimate we don't track; report 0 (its "unknown" value).
    if totals.keys > 0 {
        b.push_str(&format!(
            "db0:keys={},expires={},avg_ttl=0\r\n",
            totals.keys, totals.expires
        ));
    }
    b.push_str("\r\n");
}

// ───────────── DEBUG ─────────────

fn cmd_debug<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
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
        // OBJECT / SET-ACTIVE-EXPIRE / unknown all return +OK: DEBUG is
        // intentionally tolerant for compatibility shims.
        _ => encode_simple_string(out, "OK"),
    }
}

// ───────────── WAIT ─────────────

fn cmd_wait<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    // WAIT numreplicas timeout — single-machine kevy has zero replicas,
    // so the answer "how many replicas acked your writes" is always 0.
    // Redis blocks until numreplicas or timeout; we return immediately.
    if args.len() != 3 {
        return wrong_args(out, "wait");
    }
    encode_integer(out, 0);
}

// ───────────── SHUTDOWN ─────────────

fn cmd_shutdown<A: ArgvView + ?Sized>(args: &A, _out: &mut Vec<u8>) {
    // SHUTDOWN [NOSAVE | SAVE] — Redis exits without sending a reply
    // (client sees connection drop). v1.0 stub: parse the subcommand
    // for forward compatibility, then exit(0). Wave 2 will add the
    // AOF-flush-on-exit graceful path; for now we rely on appendfsync
    // = always or everysec to have flushed recent writes.
    let mode = args.get(1).map(<[u8]>::to_ascii_uppercase);
    let _ = mode; // accepted for parity; behavior identical for now
    std::process::exit(0);
}

// ───────────── value → string converters (shared with config submodule) ─────────────

pub(super) fn appendfsync_str(v: kevy_config::AppendFsync) -> &'static str {
    use kevy_config::AppendFsync::{Always, EverySec, No};
    match v {
        Always => "always",
        EverySec => "everysec",
        No => "no",
    }
}

pub(super) fn eviction_str(v: kevy_config::EvictionPolicy) -> &'static str {
    use kevy_config::EvictionPolicy::{NoEviction, AllKeysLru, AllKeysLfu, AllKeysRandom, VolatileLru, VolatileLfu, VolatileRandom, VolatileTtl};
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
    use kevy_config::LogLevel::{Trace, Debug, Info, Warn, Error};
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
mod tests;
