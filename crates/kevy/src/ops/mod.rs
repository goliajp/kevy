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
mod scope_move_stream;
pub(crate) mod stats;

use std::time::SystemTime;

use kevy_config::Config;
use kevy_resp::{
    ArgvView, RespVersion, encode_bulk, encode_error, encode_simple_string,
    encode_verbatim,
};
use kevy_store::Store;

use crate::state::Ctx;

/// Operational-command dispatcher. Returns `true` if the verb was
/// recognised (and a reply has been written to `out`). The config
/// snapshot is paid only inside the arms that actually need it —
/// GET / SET and the other string / collection verbs flow past via
/// the early `_ => false` without touching the config lock.
pub(crate) fn dispatch_ops<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"INFO" => cmd_info(ctx, store, args, out, RespVersion::V2),
        b"CLUSTER" => cluster::cmd_cluster(ctx, store, args, out),
        b"DEBUG" => cmd_debug(ctx, args, out),
        b"WAIT" => crate::cmd_repl::cmd_wait(&ctx.state.replication, args, out),
        b"REPL.TOKEN" => crate::cmd_repl::cmd_repl_token(&ctx.state.replication, args, out),
        b"REPL.WAIT" => crate::cmd_repl::cmd_repl_wait(&ctx.state.replication, args, out),
        b"SHUTDOWN" => cmd_shutdown(ctx, args, out),
        b"CONFIG" => config::cmd_config(ctx, args, out, RespVersion::V2),
        b"CLIENT" => client::cmd_client(args, out, RespVersion::V2),
        b"ROLE" => replication::cmd_role(ctx, args, out),
        b"REPLICAOF" | b"SLAVEOF" => replication::cmd_replicaof(ctx, args, out),
        b"MOVE-SCOPE" => scope_move::cmd_move_scope(ctx, store, args, out),
        b"MOVE-SCOPE-INGEST" => scope_move::cmd_move_scope_ingest(ctx, store, args, out),
        b"MEMORY" => {
            let cfg = ctx.state.config();
            let totals = ctx.state.obs.aggregate();
            memory::cmd_memory(&cfg, &totals, store, args, out);
        }
        _ => return false,
    }
    true
}

// ───────────── INFO ─────────────

pub(crate) fn cmd_info<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    let cfg = ctx.state.config();
    // INFO [section]; we always emit the requested section (or all when
    // none / "default" / "all" / "everything" is requested).
    let section = args.get(1).map(<[u8]>::to_ascii_lowercase);
    let want = section.as_deref();
    // Each shard owns an independent store; INFO is answered on one shard but
    // reports the whole process. Freshen this shard's slot from the live store
    // it already holds (so the answering shard is never stale, even with the
    // active reaper disabled), then sum every shard's slot.
    stats::publish_gauges(ctx.shard, store);
    let totals = ctx.state.obs.aggregate();
    let body = build_info_body(ctx, &cfg, want, &totals);
    // RESP3: Verbatim text frame (`=N\r\ntxt:<body>\r\n`) so the
    // client can render it as plain text (e.g. redis-cli prints it
    // unchanged). RESP2 stays as a length-prefixed bulk.
    match proto {
        RespVersion::V2 => encode_bulk(out, body.as_bytes()),
        RespVersion::V3 => encode_verbatim(out, *b"txt", body.as_bytes()),
    }
}

/// Assemble the INFO body — every requested section in the
/// canonical valkey order.
fn build_info_body(
    ctx: &Ctx<'_>,
    cfg: &Config,
    want: Option<&[u8]>,
    totals: &crate::state::Totals,
) -> String {
    let mut body = String::new();
    if want_section(want, "server") {
        info_server(cfg, &mut body);
    }
    if want_section(want, "clients") {
        info_clients(cfg, totals, &mut body);
    }
    if want_section(want, "memory") {
        info_memory(cfg, totals, &mut body);
    }
    // `# Tiering` (T5 / B12): present ONLY when tiering is on — an
    // untiered instance's INFO is byte-identical to pre-tiering
    // output (the transparency suite's Shape compare relies on it).
    if totals.tier_enabled && want_section(want, "tiering") {
        info_tiering(totals, &mut body);
    }
    if want_section(want, "persistence") {
        info_persistence(ctx, cfg, &mut body);
    }
    if want_section(want, "stats") {
        info_stats(ctx, totals, &mut body);
    }
    if want_section(want, "replication") {
        info_replication(ctx, &mut body);
    }
    if want_section(want, "cluster") {
        info_cluster(cfg, &mut body);
    }
    if want_section(want, "keyspace") {
        info_keyspace(totals, &mut body);
    }
    body
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

fn info_clients(cfg: &Config, totals: &crate::state::Totals, b: &mut String) {
    b.push_str("# Clients\r\n");
    // Live client conns summed over every shard's per-tick gauge
    // (stale by at most one tick interval).
    b.push_str(&format!(
        "connected_clients:{}\r\n",
        totals.clients_connected
    ));
    b.push_str(&format!("maxclients:{}\r\n", cfg.server.max_clients));
    b.push_str("\r\n");
}

fn info_memory(cfg: &Config, totals: &crate::state::Totals, b: &mut String) {
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

/// `# Tiering` (capacity arc T5, RFC §1 D3 B12): the unified-budget
/// gauges summed across shards. Emitted only when tiering is enabled —
/// see the call site's byte-stability note.
fn info_tiering(totals: &crate::state::Totals, b: &mut String) {
    let t = &totals.tier;
    b.push_str("# Tiering\r\n");
    b.push_str("tiering_enabled:1\r\n");
    b.push_str(&format!("tier_budget_bytes:{}\r\n", t.budget));
    b.push_str(&format!("tier_effective_target:{}\r\n", t.effective_target));
    b.push_str(&format!("cold_keys:{}\r\n", t.cold_keys));
    b.push_str(&format!("cold_bytes:{}\r\n", t.cold_bytes));
    b.push_str(&format!("stub_bytes:{}\r\n", t.stub_bytes));
    b.push_str(&format!("index_reserved_bytes:{}\r\n", t.reserved_bytes));
    b.push_str(&format!("vlog_size_bytes:{}\r\n", t.vlog_bytes));
    b.push_str(&format!("vlog_live_bytes:{}\r\n", t.vlog_live_bytes));
    b.push_str(&format!("vlog_files:{}\r\n", t.vlog_files));
    b.push_str(&format!("vlog_epoch:{}\r\n", t.vlog_epoch));
    b.push_str(&format!("demotions_total:{}\r\n", t.demotions_total));
    b.push_str(&format!("promotions_total:{}\r\n", t.promotions_total));
    b.push_str("\r\n");
}

fn info_persistence(ctx: &Ctx<'_>, cfg: &Config, b: &mut String) {
    // The answering shard's background-persistence view, refreshed by
    // the reactor tick via `Commands::on_persist_stats` into the shard
    // zone. Stale by at most one tick interval.
    let (in_flight, rewrites) = ctx.shard.persist_stats();
    b.push_str("# Persistence\r\n");
    // `loading` = a full-resync snapshot ship is being received (the
    // window where reads answer -LOADING). Startup AOF replay never
    // shows here — it completes before the listener accepts.
    b.push_str(&format!(
        "loading:{}\r\n",
        i32::from(ctx.state.replication.loading())
    ));
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
    // Boot-replay verdict (answering shard): bytes the startup replay had
    // to drop (quarantined + truncated), and whether the stop was a
    // corrupt frame. Non-zero dropped bytes = the shard recovered less
    // than its AOF held — alert on it.
    let (dropped, corrupt) = ctx.shard.replay_report();
    b.push_str(&format!("aof_last_open_dropped_bytes:{dropped}\r\n"));
    b.push_str(&format!("aof_last_open_corrupt:{}\r\n", i32::from(corrupt)));
    b.push_str("\r\n");
}

fn info_stats(ctx: &Ctx<'_>, totals: &crate::state::Totals, b: &mut String) {
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
        ctx.state.obs.instantaneous_ops_per_sec(totals.commands_processed)
    ));
    b.push_str(&format!("expired_keys:{}\r\n", totals.expired_keys));
    b.push_str("\r\n");
}

fn info_replication(ctx: &Ctx<'_>, b: &mut String) {
    // Live `INFO replication` — reads `current_upstream()` to decide
    // the section shape. The primary half folds every shard's view
    // slot into the instance-wide answer (offset sum, one slaveN line
    // per replica process). The fields mirror Redis 7.x, with one
    // simplification: master_replid is a single zeros-string (no
    // failover ID bookkeeping — kevy-elect keeps its own epoch
    // instead). Link status is heartbeat-derived and the per-replica
    // list is live (see the two halves below).
    b.push_str("# Replication\r\n");
    match ctx.state.replication.current_upstream() {
        Some((host, port)) => info_repl_replica(ctx, b, host, port),
        None => info_repl_master(ctx, b),
    }
    b.push_str("\r\n");
}

/// The replica-side (`role:slave`) half of `INFO replication`.
fn info_repl_replica(ctx: &Ctx<'_>, b: &mut String, host: std::net::IpAddr, port: u16) {
    b.push_str("role:slave\r\n");
    b.push_str(&format!("master_host:{host}\r\n"));
    b.push_str(&format!("master_port:{port}\r\n"));
    // Heartbeat-derived truth — link status by
    // ping freshness (<3s), applied offset and frame lag from
    // the runner registry.
    let (up, applied, lag, last_io) = ctx.state.replication.replica_link_view();
    b.push_str(if up {
        "master_link_status:up\r\n"
    } else {
        "master_link_status:down\r\n"
    });
    b.push_str(&format!("master_last_io_seconds_ago:{last_io}\r\n"));
    b.push_str("master_sync_in_progress:0\r\n");
    b.push_str(if ctx.state.replication.read_only() {
        "slave_read_only:1\r\n"
    } else {
        "slave_read_only:0\r\n"
    });
    b.push_str(&format!("slave_repl_offset:{applied}\r\n"));
    b.push_str(&format!("slave_lag_frames:{lag}\r\n"));
}

/// The primary-side (`role:master`) half of `INFO replication` —
/// instance-wide: every shard's view slot folded into one offset sum
/// and one `slaveN` line per replica process.
fn info_repl_master(ctx: &Ctx<'_>, b: &mut String) {
    let views = ctx.state.obs.repl_views();
    let (offset, replicas) = replication::aggregate_replication(&views);
    b.push_str("role:master\r\n");
    b.push_str(&format!("connected_slaves:{}\r\n", replicas.len()));
    // Per-replica truth — port is the replica's advertised client
    // port; sent (pumped) / offset (acked) / lag sum its per-shard
    // streams; a replica missing an ACK on ANY stream is `syncing`.
    for (i, agg) in replicas.iter().enumerate() {
        let acked_v = agg.acked.unwrap_or(0);
        let lag = offset.saturating_sub(acked_v);
        let state = if agg.acked.is_some() { "online" } else { "syncing" };
        b.push_str(&format!(
            "slave{i}:ip={},port={},state={state},offset={acked_v},sent={},lag={lag}\r\n",
            agg.ip, agg.port, agg.sent,
        ));
    }
    b.push_str("master_replid:0000000000000000000000000000000000000000\r\n");
    b.push_str(&format!("master_repl_offset:{offset}\r\n"));
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

fn info_keyspace(totals: &crate::state::Totals, b: &mut String) {
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

fn cmd_debug<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "debug"),
    };
    // Audit every DEBUG call (admin command).
    let mut event: Vec<&[u8]> = Vec::with_capacity(args.len());
    event.push(b"DEBUG");
    for i in 1..args.len() {
        event.push(&args[i]);
    }
    ctx.state.obs.audit_record(&event);
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

// WAIT lives in crate::cmd_repl (real all-shard ack
// barrier through the runtime; the dispatch fallback there handles
// arity errors, the replica rejection, and runtime-less contexts).

// ───────────── SHUTDOWN ─────────────

fn cmd_shutdown<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    // SHUTDOWN [NOSAVE | SAVE] — a successful shutdown never sends a
    // reply: the client observes the connection closing as the process
    // drains and exits (Redis behavior). The command trips the same
    // stop flag the SIGTERM handler uses, so every shard leaves its
    // reactor loop and runs the full drain: land in-flight persist
    // jobs, force-fsync the AOF tail, write the feed marker. `SAVE`
    // additionally requests one final snapshot per shard before the
    // drain; `NOSAVE` (and the bare form) skip the snapshot but keep
    // the AOF durable.
    if args.len() > 2 {
        return encode_error(out, "ERR syntax error");
    }
    let save = match args.get(1).map(<[u8]>::to_ascii_uppercase).as_deref() {
        None => false,
        Some(b"NOSAVE") => false,
        Some(b"SAVE") => true,
        Some(_) => return encode_error(out, "ERR syntax error"),
    };
    if !ctx.state.request_shutdown(save) {
        // No registered runtime stop flag (embedded / bare-dispatch
        // contexts): keep the immediate-exit contract.
        std::process::exit(0);
    }
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
