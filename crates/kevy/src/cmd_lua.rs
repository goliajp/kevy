//! `EVAL` / `EVALSHA` / `EVAL_RO` / `EVALSHA_RO` / `SCRIPT` command
//! handlers. v1.27 P7b — wires kevy-lua-host's `LuaHost<Store>` into
//! the kevy dispatch table.
//!
//! ## Per-shard `LuaHost`
//!
//! `LuaHost<Store>` is created lazily on the first EVAL hitting a
//! given thread and reused thereafter (one per shard, since kevy is
//! thread-per-core). The host is held in a `thread_local!
//! RefCell<Option<LuaHost<Store>>>` — `Option` so we can lazy-init,
//! `RefCell` so we can `borrow_mut` for the eval duration.
//!
//! ## Re-entrancy: EVAL inside EVAL → `-ERR`
//!
//! Nested EVAL (a Lua script calls `redis.call('EVAL', ...)` which
//! routes back through `dispatch_lua`) hits a `try_borrow_mut`
//! failure on the per-shard host and surfaces as
//! `-ERR EVAL inside EVAL is not supported in v1.27`. Real Redis
//! doesn't permit nested EVAL either (the inner call gets a similar
//! error). Lifting this is on the v1.28+ backlog if a real workload
//! ever needs it.

use crate::cmd::wrong_args;
use kevy_lua_host::LuaHost;
use kevy_resp::{Argv, ArgvView, encode_error};
use kevy_store::Store;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

thread_local! {
    /// Per-shard (= per-thread, kevy is thread-per-core) Lua host.
    /// Lazily constructed on first EVAL. Lives until the thread
    /// exits.
    static LUA_HOST: RefCell<Option<LuaHost<Store>>> = const { RefCell::new(None) };
}

/// v1.27.1 fix for multi-shard EVAL routing: process-global script
/// cache shared across all shards, replacing v1.27.0's per-Bridge
/// cache. SCRIPT LOAD writes here; EVALSHA reads here and forwards
/// the source to `LuaHost::eval` so the per-shard VM pool still runs
/// the script (thread-locality preserved). SCRIPT EXISTS / FLUSH
/// also hit the global.
///
/// The previous v1.27.0 design kept the cache inside `Bridge`, which
/// was per-shard via `thread_local LUA_HOST`. That meant `SCRIPT LOAD`
/// arriving on shard X only filled X's cache, and `EVALSHA` arriving
/// on shard Y missed and returned `-NOSCRIPT`. Now the cache is
/// process-wide; routing the EVAL itself to KEYS[1]'s shard is done
/// at `Commands::route` time (see [`crate::commands`]).
static SCRIPT_CACHE: OnceLock<Mutex<HashMap<[u8; 20], Vec<u8>>>> = OnceLock::new();

fn script_cache() -> &'static Mutex<HashMap<[u8; 20], Vec<u8>>> {
    SCRIPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build a `LuaHost<Store>` whose dispatch closure routes redis.call
/// argv through `kevy::dispatch::dispatch_into` against the host
/// `&mut Store`.
fn make_lua_host() -> LuaHost<Store> {
    let mut host = LuaHost::<Store>::new(|store, argv, read_only| {
        // P7c: read-only enforcement. EVAL_RO / EVALSHA_RO set
        // read_only=true; reject writes per Redis semantics.
        if read_only {
            if let Some(cmd) = argv.first() {
                let upper: Vec<u8> = cmd.iter().map(|b| b.to_ascii_uppercase()).collect();
                if crate::cmd::is_write_verb(&upper) {
                    return b"-READONLY can't write against a read-only script\r\n".to_vec();
                }
            }
        }
        let mut a = Argv::default();
        for slice in argv {
            a.push(slice);
        }
        let mut out = Vec::new();
        crate::dispatch::dispatch_into(store, &a, &mut out);
        // v1.27.3: bridge inner-EVAL writes to the runtime's BLOCK
        // wake hook. `dispatch_into` hits the Store directly and
        // bypasses `post_write_housekeeping` where wake_key normally
        // fires. Push the affected key to a thread-local buffer so
        // the runtime drains + wakes after the outer EVAL returns
        // (see kevy_rt::lua_wake_bridge). Cheap: one match + push.
        if !out.is_empty()
            && out[0] != b'-'
            && let Some(verb) = argv.first()
        {
            let mut buf = [0u8; 32];
            let upper = crate::cmd::upper_verb(verb, &mut buf);
            if matches!(
                upper,
                b"LPUSH" | b"RPUSH" | b"XADD" | b"ZADD" | b"ZINCRBY"
            ) && let Some(key) = argv.get(1)
            {
                kevy_rt::push_lua_wake_key(key);
            }
        }
        out
    });
    // v1.27 P7e: read `[lua] time_limit_ms` + `[lua] allow_dialects`
    // from the process-wide config at first-EVAL time. Operators who
    // hot-reload `[lua]` settings after the first EVAL need to also
    // SCRIPT FLUSH (drops the per-dialect Vm pool) or restart the
    // server — v1.28 backlog if there's real demand.
    let cfg = crate::config_global::get();
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
    host
}

/// Run `f` with the per-shard `LuaHost`. Returns `None` if the host
/// is already borrowed (re-entrant EVAL).
fn with_host<R>(f: impl FnOnce(&mut LuaHost<Store>) -> R) -> Option<R> {
    LUA_HOST.with(|h| match h.try_borrow_mut() {
        Ok(mut g) => Some(f(g.get_or_insert_with(make_lua_host))),
        Err(_) => None,
    })
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
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"EVAL" => {
            cmd_eval(store, args, out, /* read_only */ false);
            true
        }
        b"EVAL_RO" => {
            cmd_eval(store, args, out, /* read_only */ true);
            true
        }
        b"EVALSHA" => {
            cmd_evalsha(store, args, out, /* read_only */ false);
            true
        }
        b"EVALSHA_RO" => {
            cmd_evalsha(store, args, out, /* read_only */ true);
            true
        }
        b"SCRIPT" => {
            cmd_script(args, out);
            true
        }
        _ => false,
    }
}

// ─────────────────────────────────────────────────────────────────────
// EVAL / EVAL_RO
// ─────────────────────────────────────────────────────────────────────

fn cmd_eval<A: ArgvView + ?Sized>(
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
    let numkeys: usize = match parse_uint(args.get(2).unwrap_or(b"")) {
        Some(n) => n,
        None => {
            encode_error(out, "ERR value is not an integer or out of range");
            return;
        }
    };
    let total_after_numkeys = args.len().saturating_sub(3);
    if numkeys > total_after_numkeys {
        encode_error(
            out,
            "ERR Number of keys can't be greater than number of args",
        );
        return;
    }
    let keys: Vec<&[u8]> = (0..numkeys)
        .map(|i| args.get(3 + i).unwrap_or(b""))
        .collect();
    let argv: Vec<&[u8]> = ((3 + numkeys)..args.len())
        .map(|i| args.get(i).unwrap_or(b""))
        .collect();
    if let Some(crossslot) = cross_slot_check(&keys) {
        out.extend_from_slice(&crossslot);
        return;
    }
    // v1.27.1: also push the script into the process-global SCRIPT
    // cache so a subsequent EVALSHA from any shard finds it (matches
    // Redis's auto-cache-on-EVAL semantics).
    let sha = kevy_lua::sha1::sha1(script);
    script_cache().lock().unwrap().insert(sha, script.to_vec());
    let reply = with_host(|h| {
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
    let numkeys: usize = match parse_uint(args.get(2).unwrap_or(b"")) {
        Some(n) => n,
        None => {
            encode_error(out, "ERR value is not an integer or out of range");
            return;
        }
    };
    let total_after_numkeys = args.len().saturating_sub(3);
    if numkeys > total_after_numkeys {
        encode_error(
            out,
            "ERR Number of keys can't be greater than number of args",
        );
        return;
    }
    let keys: Vec<&[u8]> = (0..numkeys)
        .map(|i| args.get(3 + i).unwrap_or(b""))
        .collect();
    let argv: Vec<&[u8]> = ((3 + numkeys)..args.len())
        .map(|i| args.get(i).unwrap_or(b""))
        .collect();
    if let Some(crossslot) = cross_slot_check(&keys) {
        out.extend_from_slice(&crossslot);
        return;
    }
    // v1.27.1: lookup the script source from the process-global
    // cache (any shard's SCRIPT LOAD / EVAL filled it). Bypass
    // `LuaHost::evalsha` whose per-Bridge cache only sees the local
    // shard's history.
    let source = match script_cache().lock().unwrap().get(&sha).cloned() {
        Some(s) => s,
        None => {
            encode_error(out, "NOSCRIPT No matching script. Please use EVAL.");
            return;
        }
    };
    let reply = with_host(|h| {
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

fn cmd_script<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
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
    // v1.27.1: SCRIPT LOAD / EXISTS / FLUSH operate on the
    // process-global cache; no per-shard LuaHost touched, so no
    // re-entrancy guard needed and no shard-local state to worry
    // about under multi-shard configs.
    match sub_upper.as_slice() {
        b"LOAD" => script_load(args, out),
        b"EXISTS" => script_exists(args, out),
        b"FLUSH" => script_flush(args, out),
        _ => encode_error(
            out,
            "ERR SCRIPT subcommand must be one of LOAD, EXISTS, FLUSH",
        ),
    }
}

fn script_load<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 3 {
        wrong_args(out, "script|load");
        return;
    }
    let source = args.get(2).unwrap_or(b"");
    let sha = kevy_lua::sha1::sha1(source);
    script_cache().lock().unwrap().insert(sha, source.to_vec());
    let hex = kevy_lua::sha1::hex(&sha);
    out.push(b'$');
    out.extend_from_slice(b"40\r\n");
    out.extend_from_slice(&hex);
    out.extend_from_slice(b"\r\n");
}

fn script_exists<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        wrong_args(out, "script|exists");
        return;
    }
    let cache = script_cache().lock().unwrap();
    let count = args.len() - 2;
    out.extend_from_slice(format!("*{count}\r\n").as_bytes());
    for i in 2..args.len() {
        let hit = kevy_lua::sha1::parse_hex(args.get(i).unwrap_or(b""))
            .is_some_and(|sha| cache.contains_key(&sha));
        out.extend_from_slice(if hit { b":1\r\n" } else { b":0\r\n" });
    }
}

fn script_flush<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    // Accept both `SCRIPT FLUSH` and `SCRIPT FLUSH SYNC|ASYNC`. The
    // mode tag is parsed/validated but currently both run
    // synchronously (in-memory cache clear is instant; v1.28 may
    // differentiate when real async cleanup arrives).
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
    script_cache().lock().unwrap().clear();
    out.extend_from_slice(b"+OK\r\n");
}

// ─────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────

fn parse_uint(bytes: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(bytes).ok()?;
    let n: i64 = s.parse().ok()?;
    if n < 0 { None } else { Some(n as usize) }
}

/// v1.27 P7d: cluster-mode cross-slot check.
///
/// When `[cluster] enabled = true`, every key in a single EVAL /
/// EVALSHA must hash to the same CRC16 slot — same constraint kevy
/// already enforces for built-in multi-key commands at the cluster
/// port. Returns `Some(-CROSSSLOT ...)` reply if the keys disagree;
/// `None` when the check passes (single-key, empty-keys, or cluster
/// mode off).
///
/// Single-shard mode (the v1.x default) skips the check entirely so
/// non-cluster operators keep their existing behaviour.
fn cross_slot_check(keys: &[&[u8]]) -> Option<Vec<u8>> {
    if keys.len() < 2 {
        return None;
    }
    let cfg = crate::config_global::get();
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
