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

thread_local! {
    /// Per-shard (= per-thread, kevy is thread-per-core) Lua host.
    /// Lazily constructed on first EVAL. Lives until the thread
    /// exits.
    static LUA_HOST: RefCell<Option<LuaHost<Store>>> = const { RefCell::new(None) };
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
    let reply = with_host(|h| {
        if read_only {
            h.evalsha_ro(store, sha, &keys, &argv)
        } else {
            h.evalsha(store, sha, &keys, &argv)
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
    let r = with_host(|h| match sub_upper.as_slice() {
        b"LOAD" => script_load(h, args),
        b"EXISTS" => script_exists(h, args),
        b"FLUSH" => script_flush(h, args),
        _ => Vec::new(), // sentinel: unknown subcommand handled below
    });
    match r {
        Some(bytes) if !bytes.is_empty() => out.extend_from_slice(&bytes),
        Some(_) => {
            encode_error(
                out,
                "ERR SCRIPT subcommand must be one of LOAD, EXISTS, FLUSH",
            );
        }
        None => emit_reentry_err(out),
    }
}

fn script_load<A: ArgvView + ?Sized>(h: &mut LuaHost<Store>, args: &A) -> Vec<u8> {
    if args.len() != 3 {
        let mut out = Vec::new();
        wrong_args(&mut out, "script|load");
        return out;
    }
    let sha = h.script_load(args.get(2).unwrap_or(b""));
    let hex = kevy_lua::sha1::hex(&sha);
    let mut out = Vec::with_capacity(50);
    out.push(b'$');
    out.extend_from_slice(b"40\r\n");
    out.extend_from_slice(&hex);
    out.extend_from_slice(b"\r\n");
    out
}

fn script_exists<A: ArgvView + ?Sized>(h: &mut LuaHost<Store>, args: &A) -> Vec<u8> {
    if args.len() < 3 {
        let mut out = Vec::new();
        wrong_args(&mut out, "script|exists");
        return out;
    }
    let mut shas: Vec<[u8; 20]> = Vec::with_capacity(args.len() - 2);
    for i in 2..args.len() {
        match kevy_lua::sha1::parse_hex(args.get(i).unwrap_or(b"")) {
            Some(s) => shas.push(s),
            None => shas.push([0u8; 20]), // malformed hex → never in cache
        }
    }
    let hits = h.script_exists(&shas);
    let mut out = Vec::with_capacity(8 + hits.len() * 4);
    out.extend_from_slice(format!("*{}\r\n", hits.len()).as_bytes());
    for hit in hits {
        out.extend_from_slice(if hit { b":1\r\n" } else { b":0\r\n" });
    }
    out
}

fn script_flush<A: ArgvView + ?Sized>(h: &mut LuaHost<Store>, args: &A) -> Vec<u8> {
    // Accept both `SCRIPT FLUSH` and `SCRIPT FLUSH SYNC|ASYNC`.
    let mode = if args.len() == 2 {
        kevy_lua::FlushMode::Sync
    } else if args.len() == 3 {
        match args.get(2).unwrap_or(b"") {
            s if s.eq_ignore_ascii_case(b"SYNC") => kevy_lua::FlushMode::Sync,
            s if s.eq_ignore_ascii_case(b"ASYNC") => kevy_lua::FlushMode::Async,
            _ => {
                let mut out = Vec::new();
                encode_error(&mut out, "ERR SCRIPT FLUSH mode must be SYNC or ASYNC");
                return out;
            }
        }
    } else {
        let mut out = Vec::new();
        wrong_args(&mut out, "script|flush");
        return out;
    };
    h.script_flush(mode);
    b"+OK\r\n".to_vec()
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
