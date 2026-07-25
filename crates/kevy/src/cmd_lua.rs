//! `EVAL` / `EVALSHA` / `EVAL_RO` / `EVALSHA_RO` / `SCRIPT` command
//! handlers — wires kevy-lua-host's `LuaHost<Store>` into
//! the kevy dispatch table.
//!
//! ## Per-shard `LuaHost`
//!
//! `LuaHost<Store>` is created lazily on the first EVAL hitting a
//! given shard and reused thereafter. It cannot live in the shard
//! zone (`ShardCtx`): a `LuaHost` is `!Send` (luna-core's `Vm` holds `Rc`s
//! and raw GC pointers) while `kevy_rt::Commands` requires `Send`, so
//! the host parks in [`kevy_lua_host::with_thread_host`]'s per-thread
//! slot — thread == shard under thread-per-core, same isolation. The
//! dispatch closure captures `Arc<RuntimeState>` + the owning shard's
//! id at build time.
//!
//! ## Re-entrancy: EVAL inside EVAL → `-ERR`
//!
//! Nested EVAL (a Lua script calls `redis.call('EVAL', ...)` which
//! routes back through `dispatch_lua`) finds the thread slot already
//! borrowed and surfaces as
//! an `-ERR EVAL inside EVAL is not supported…` error. Real Redis
//! doesn't permit nested EVAL either (the inner call gets a similar
//! error).

use crate::cmd::wrong_args;
use crate::state::{Ctx, RuntimeState, ShardCtx};
use kevy_lua_host::LuaHost;
use kevy_resp::{Argv, ArgvView, encode_error};
use kevy_store::Store;
use std::sync::Arc;

/// Build a `LuaHost<Store>` whose dispatch closure routes redis.call
/// argv through `crate::dispatch::dispatch_into` against the host
/// `&mut Store`. `my_shard` is the owning shard's id, captured for
/// the cross-shard target check (the closure outlives any one `Ctx`).
fn make_lua_host(state: Arc<RuntimeState>, my_shard: usize) -> LuaHost<Store> {
    let cfg = state.config();
    let mut host = LuaHost::<Store>::new(move |store, argv, read_only| {
        lua_redis_call(&state, my_shard, store, argv, read_only)
    });
    apply_lua_config(&mut host, &cfg);
    host
}

/// The `redis.call` dispatch closure body: enforcement checks, then
/// route the inner argv through `crate::dispatch::dispatch_into`.
/// EVAL_RO / EVALSHA_RO write rejection: a read-only script calling
/// a write verb answers -READONLY (Redis semantics).
fn read_only_violation(argv: &[&[u8]], read_only: bool) -> Option<Vec<u8>> {
    if !read_only {
        return None;
    }
    let cmd = argv.first()?;
    let mut buf = [0u8; 32];
    if crate::cmd::is_write_verb(crate::cmd::upper_verb(cmd, &mut buf)) {
        return Some(b"-READONLY can't write against a read-only script\r\n".to_vec());
    }
    None
}

/// Cross-shard inner-call enforcement. Under `--threads > 1`, EVAL
/// routes to KEYS[1]'s shard; the inner `redis.call` hits this same
/// shard's Store. Calling on a key that lives on a different shard
/// silently mis-routes — matches Redis Cluster's intended-disallowed
/// behaviour, enforced loudly with CROSSSLOT instead of corrupting
/// state (the same rule Redis Cluster applies via slot validation).
fn cross_shard_violation(
    state: &Arc<RuntimeState>,
    my_shard: usize,
    argv: &[&[u8]],
) -> Option<Vec<u8>> {
    let cfg = state.config();
    let nshards = cfg.server.threads;
    if nshards > 1
        && let Some(target_key) = argv.get(1)
    {
        let target_shard =
            kevy_rt::shard_of_key(target_key, nshards, cfg.cluster.enabled);
        if target_shard != my_shard {
            return Some(b"-CROSSSLOT Lua redis.call target key is on a different shard than the EVAL. Use {hashtag} to colocate keys, or run kevy --threads 1.\r\n".to_vec());
        }
    }
    None
}

fn lua_redis_call(
    state: &Arc<RuntimeState>,
    my_shard: usize,
    store: &mut Store,
    argv: &[&[u8]],
    read_only: bool,
) -> Vec<u8> {
    if let Some(err) = read_only_violation(argv, read_only) {
        return err;
    }
    if let Some(err) = cross_shard_violation(state, my_shard, argv) {
        return err;
    }
    let mut a = Argv::default();
    for slice in argv {
        a.push(slice);
    }
    let mut out = Vec::new();
    // Inner calls dispatch with a fresh shard zone (the outer zone's
    // Lua host is borrowed for the eval duration): the only shard-
    // private state a redis.call-able verb can touch is the MOVE-
    // SCOPE-INGEST prefix, and ingest bulks never contain EVAL. The
    // shard id carries over so shard-identity readers (CLUSTER MYID)
    // answer as the real shard.
    let shard = ShardCtx::default();
    shard.set_shard_id(my_shard);
    let ctx = Ctx { state, shard: &shard };
    crate::dispatch::dispatch_into(&ctx, store, &a, &mut out);
    // The runtime logs/replicates the OUTER EVAL frame (whole-script
    // propagation); an inner nondeterministic verb (SPOP) must not
    // leak its effect-frame override into the EVAL's own post-write
    // housekeeping — that would replace the whole script's frame with
    // a single SREM. Drop it here, per inner call.
    kevy_rt::propagation::discard_override();
    bridge_lua_wake_keys(argv, &out);
    out
}

/// Bridge inner-EVAL writes to the runtime's BLOCK
/// wake hook. `dispatch_into` hits the Store directly and
/// bypasses `post_write_housekeeping` where wake_key normally
/// fires. Push the affected key to a thread-local buffer so
/// the runtime drains + wakes after the outer EVAL returns
/// (see kevy_rt::lua_wake_bridge). Cheap: one match + push.
fn bridge_lua_wake_keys(argv: &[&[u8]], out: &[u8]) {
    if !out.is_empty()
        && out[0] != b'-'
        && let Some(verb) = argv.first()
    {
        let mut buf = [0u8; 32];
        let upper = crate::cmd::upper_verb(verb, &mut buf);
        // Single source: the wake set lives in cmd_block (grounded
        // against the OP_TABLE); this used to be a hand-copied list.
        if crate::cmd_block::wake_idx_for_verb(upper).is_some()
            && let Some(key) = argv.get(1)
        {
            kevy_rt::push_lua_wake_key(key);
        }
    }
}

/// Read `[lua] time_limit_ms` + `[lua] allow_dialects`
/// from the live config at first-EVAL time. Operators who
/// hot-reload `[lua]` settings after the first EVAL need to also
/// SCRIPT FLUSH (drops the per-dialect Vm pool) or restart the
/// server.
fn apply_lua_config(host: &mut LuaHost<Store>, cfg: &kevy_config::Config) {
    // Translate ms → instruction budget. Rough conservative
    // calibration: 40 000 instr/ms on M-series hardware (the same
    // ratio implied by the original 200 M / 5000 ms default).
    if cfg.lua.time_limit_ms > 0 {
        let budget = (cfg.lua.time_limit_ms as i64).saturating_mul(40_000);
        host.set_instr_budget(budget);
    } else {
        host.set_instr_budget(0); // unlimited
    }
    if !cfg.lua.allow_dialects.is_empty() {
        let versions: Vec<kevy_lua::LuaVersion> = cfg
            .lua
            .allow_dialects
            .iter()
            .filter_map(|s| match s.as_str() {
                "5.1" | "51" => Some(kevy_lua::LuaVersion::Lua51),
                "5.2" | "52" => Some(kevy_lua::LuaVersion::Lua52),
                "5.3" | "53" => Some(kevy_lua::LuaVersion::Lua53),
                "5.4" | "54" => Some(kevy_lua::LuaVersion::Lua54),
                "5.5" | "55" => Some(kevy_lua::LuaVersion::Lua55),
                _ => None,
            })
            .collect();
        if !versions.is_empty() {
            host.set_allowed_dialects(&versions);
        }
    }
}

/// Run `f` with the per-shard `LuaHost` (parked in the kevy-lua-host
/// thread slot — see the module doc). Returns `None` if the host is
/// already borrowed (re-entrant EVAL).
fn with_host<R>(ctx: &Ctx<'_>, f: impl FnOnce(&mut LuaHost<Store>) -> R) -> Option<R> {
    kevy_lua_host::with_thread_host(
        || make_lua_host(Arc::clone(ctx.state), ctx.shard.shard_id()),
        f,
    )
}

fn emit_reentry_err(out: &mut Vec<u8>) {
    encode_error(
        out,
        "ERR EVAL inside EVAL is not supported in v1.27",
    );
}

/// Dispatch entry for Lua-scripting commands. Returns `true` when
/// the command was recognised (handler ran, reply appended to
/// `out`).
pub(crate) fn dispatch_lua<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"EVAL" => {
            cmd_eval(ctx, store, args, out, /* read_only */ false);
            true
        }
        b"EVAL_RO" => {
            cmd_eval(ctx, store, args, out, /* read_only */ true);
            true
        }
        b"EVALSHA" => {
            cmd_evalsha(ctx, store, args, out, /* read_only */ false);
            true
        }
        b"EVALSHA_RO" => {
            cmd_evalsha(ctx, store, args, out, /* read_only */ true);
            true
        }
        b"SCRIPT" => {
            cmd_script(ctx, args, out);
            true
        }
        _ => false,
    }
}

// ─────────────────────────────────────────────────────────────────────
// EVAL / EVAL_RO
// ─────────────────────────────────────────────────────────────────────

fn cmd_eval<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
) {
    if args.len() < 3 {
        wrong_args(out, if read_only { "eval_ro" } else { "eval" });
        return;
    }
    let script: &[u8] = args.get(1).unwrap_or(b"");
    let Some((keys, argv)) = parse_eval_keys_argv(ctx, args, out) else {
        return;
    };
    // Also push the script into the shared SCRIPT cache so a
    // subsequent EVALSHA from any shard finds it (matches Redis's
    // auto-cache-on-EVAL semantics).
    let sha = kevy_lua::sha1::sha1(script);
    ctx.state.catalogs.scripts.lock().unwrap().insert(sha, script.to_vec());
    let reply = with_host(ctx, |h| {
        if read_only {
            h.eval_ro(store, script, &keys, &argv)
        } else {
            h.eval(store, script, &keys, &argv)
        }
    });
    match reply {
        Some(bytes) => out.extend_from_slice(&bytes),
        None => emit_reentry_err(out),
    }
}

// ─────────────────────────────────────────────────────────────────────
// EVALSHA / EVALSHA_RO
// ─────────────────────────────────────────────────────────────────────

fn cmd_evalsha<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
) {
    if args.len() < 3 {
        wrong_args(out, if read_only { "evalsha_ro" } else { "evalsha" });
        return;
    }
    let sha_hex: &[u8] = args.get(1).unwrap_or(b"");
    let sha = match kevy_lua::sha1::parse_hex(sha_hex) {
        Some(s) => s,
        None => {
            encode_error(out, "NOSCRIPT No matching script. Please use EVAL.");
            return;
        }
    };
    let Some((keys, argv)) = parse_eval_keys_argv(ctx, args, out) else {
        return;
    };
    // Lookup the script source from the shared cache (any
    // shard's SCRIPT LOAD / EVAL filled it). Bypass
    // `LuaHost::evalsha` whose per-Bridge cache only sees the local
    // shard's history.
    let source = match ctx.state.catalogs.scripts.lock().unwrap().get(&sha).cloned() {
        Some(s) => s,
        None => {
            encode_error(out, "NOSCRIPT No matching script. Please use EVAL.");
            return;
        }
    };
    let reply = with_host(ctx, |h| {
        if read_only {
            h.eval_ro(store, &source, &keys, &argv)
        } else {
            h.eval(store, &source, &keys, &argv)
        }
    });
    match reply {
        Some(bytes) => out.extend_from_slice(&bytes),
        None => emit_reentry_err(out),
    }
}

// ─────────────────────────────────────────────────────────────────────
// SCRIPT subcommands
// ─────────────────────────────────────────────────────────────────────

fn cmd_script<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() < 2 {
        wrong_args(out, "script");
        return;
    }
    let sub_upper: Vec<u8> = args
        .get(1)
        .unwrap_or(b"")
        .iter()
        .map(|b| b.to_ascii_uppercase())
        .collect();
    // SCRIPT LOAD / EXISTS / FLUSH operate on the shared
    // cache; no per-shard LuaHost touched, so no re-entrancy guard
    // needed and no shard-local state to worry about under
    // multi-shard configs.
    match sub_upper.as_slice() {
        b"LOAD" => script_load(ctx, args, out),
        b"EXISTS" => script_exists(ctx, args, out),
        b"FLUSH" => script_flush(ctx, args, out),
        _ => encode_error(
            out,
            "ERR SCRIPT subcommand must be one of LOAD, EXISTS, FLUSH",
        ),
    }
}

fn script_load<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 3 {
        wrong_args(out, "script|load");
        return;
    }
    let source = args.get(2).unwrap_or(b"");
    let sha = kevy_lua::sha1::sha1(source);
    ctx.state.catalogs.scripts.lock().unwrap().insert(sha, source.to_vec());
    let hex = kevy_lua::sha1::hex(&sha);
    out.push(b'$');
    out.extend_from_slice(b"40\r\n");
    out.extend_from_slice(&hex);
    out.extend_from_slice(b"\r\n");
}

fn script_exists<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        wrong_args(out, "script|exists");
        return;
    }
    let cache = ctx.state.catalogs.scripts.lock().unwrap();
    let count = args.len() - 2;
    out.extend_from_slice(format!("*{count}\r\n").as_bytes());
    for i in 2..args.len() {
        let hit = kevy_lua::sha1::parse_hex(args.get(i).unwrap_or(b""))
            .is_some_and(|sha| cache.contains_key(&sha));
        out.extend_from_slice(if hit { b":1\r\n" } else { b":0\r\n" });
    }
}

fn script_flush<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    // Accept both `SCRIPT FLUSH` and `SCRIPT FLUSH SYNC|ASYNC`. The
    // mode tag is parsed/validated but currently both run
    // synchronously (an in-memory cache clear is instant, so there is
    // nothing to defer).
    if args.len() == 3 {
        let mode = args.get(2).unwrap_or(b"");
        if !mode.eq_ignore_ascii_case(b"SYNC") && !mode.eq_ignore_ascii_case(b"ASYNC") {
            encode_error(out, "ERR SCRIPT FLUSH mode must be SYNC or ASYNC");
            return;
        }
    } else if args.len() != 2 {
        wrong_args(out, "script|flush");
        return;
    }
    ctx.state.catalogs.scripts.lock().unwrap().clear();
    out.extend_from_slice(b"+OK\r\n");
}

// ─────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────

/// Shared EVAL / EVALSHA prelude: parse `numkeys`, collect the
/// `KEYS` / `ARGV` slices, and run the cluster cross-slot check.
/// KEYS / ARGV borrowed slices for one EVAL invocation.
type KeysArgv<'a> = (Vec<&'a [u8]>, Vec<&'a [u8]>);

/// `None` = an error reply was already appended to `out`.
fn parse_eval_keys_argv<'a, A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    args: &'a A,
    out: &mut Vec<u8>,
) -> Option<KeysArgv<'a>> {
    let numkeys: usize = match parse_uint(args.get(2).unwrap_or(b"")) {
        Some(n) => n,
        None => {
            encode_error(out, "ERR value is not an integer or out of range");
            return None;
        }
    };
    let total_after_numkeys = args.len().saturating_sub(3);
    if numkeys > total_after_numkeys {
        encode_error(
            out,
            "ERR Number of keys can't be greater than number of args",
        );
        return None;
    }
    let keys: Vec<&[u8]> = (0..numkeys)
        .map(|i| args.get(3 + i).unwrap_or(b""))
        .collect();
    let argv: Vec<&[u8]> = ((3 + numkeys)..args.len())
        .map(|i| args.get(i).unwrap_or(b""))
        .collect();
    if let Some(crossslot) = cross_slot_check(ctx, &keys) {
        out.extend_from_slice(&crossslot);
        return None;
    }
    Some((keys, argv))
}

fn parse_uint(bytes: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(bytes).ok()?;
    let n: i64 = s.parse().ok()?;
    if n < 0 { None } else { Some(n as usize) }
}

/// Cluster-mode cross-slot check.
///
/// When `[cluster] enabled = true`, every key in a single EVAL /
/// EVALSHA must hash to the same CRC16 slot — same constraint kevy
/// already enforces for built-in multi-key commands at the cluster
/// port. Returns `Some(-CROSSSLOT ...)` reply if the keys disagree;
/// `None` when the check passes (single-key, empty-keys, or cluster
/// mode off).
///
/// Single-shard mode (the default) skips the check entirely so
/// non-cluster operators keep their existing behaviour.
fn cross_slot_check(ctx: &Ctx<'_>, keys: &[&[u8]]) -> Option<Vec<u8>> {
    if keys.len() < 2 {
        return None;
    }
    let cfg = ctx.state.config();
    if !cfg.cluster.enabled {
        return None;
    }
    let first = kevy_hash::key_hash_slot(keys[0]);
    for k in &keys[1..] {
        if kevy_hash::key_hash_slot(k) != first {
            let mut out = Vec::new();
            encode_error(
                &mut out,
                "CROSSSLOT Keys in request don't hash to the same slot",
            );
            return Some(out);
        }
    }
    None
}
