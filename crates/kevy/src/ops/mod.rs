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
mod scope_move_emit;
mod scope_move_ingest;
mod scope_move_stream;
pub(crate) mod stats;
mod info_sections;
use info_sections::*;

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
    // `# Tiering`: present ONLY when tiering is on — an
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
    if want_section(want, "modules") {
        info_modules(totals, &mut body);
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
