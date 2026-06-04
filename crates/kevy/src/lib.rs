//! kevy — a single-machine, Redis-compatible key–value server.
//!
//! This crate is the server: it supplies the command *semantics* — routing
//! ([`KevyCommands`]) and execution ([`dispatch`]) — and wires them to the
//! [kevy-rt] shared-nothing thread-per-core runtime via [`serve`]. The command
//! logic is also reachable directly (one keyspace, no I/O) through [`dispatch`],
//! which is handy for embedding or testing. Built from a small stack of
//! zero-dependency crates: [kevy-sys], [kevy-resp], [kevy-store], [kevy-net],
//! [kevy-rt], [kevy-persist].
//!
//! [kevy-rt]: https://crates.io/crates/kevy-rt
//! [kevy-sys]: https://crates.io/crates/kevy-sys
//! [kevy-resp]: https://crates.io/crates/kevy-resp
//! [kevy-store]: https://crates.io/crates/kevy-store
//! [kevy-net]: https://crates.io/crates/kevy-net
//! [kevy-persist]: https://crates.io/crates/kevy-persist
//!
//! # Example
//!
//! Run commands against an in-process keyspace (no sockets):
//!
//! ```
//! use kevy::{Argv, KeyspaceStore, dispatch};
//!
//! let mut store = KeyspaceStore::new();
//! let cmd = |parts: &[&[u8]]| Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
//! assert_eq!(dispatch(&mut store, &cmd(&[b"SET", b"k", b"v"])), b"+OK\r\n");
//! assert_eq!(dispatch(&mut store, &cmd(&[b"GET", b"k"])), b"$1\r\nv\r\n");
//! assert_eq!(dispatch(&mut store, &cmd(&[b"INCR", b"n"])), b":1\r\n");
//! ```
//!
//! To run the full server: [`serve`]`(ip, port, nshards, dir, aof)`.
#![forbid(unsafe_code)]

use kevy_resp::{
    encode_array_len, encode_bulk, encode_error, encode_integer, encode_map_header, parse_command,
};
use kevy_rt::{ArgvView, Commands, ResolvedCmd, RespVersion, Route, Runtime, TxnKind};
use kevy_store::Store;
use kevy_sys::Socket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod cmd;
mod cmd_data;
mod config_global;
mod dispatch;
mod dispatch_collections;
mod ops;

pub use config_global::init as config_init;
pub use config_global::replace as config_replace;
use cmd::{scan_pattern, upper_verb};
pub use dispatch::dispatch;
pub use kevy_rt::Argv;
pub use kevy_store::Store as KeyspaceStore;

/// What to do with a connection after draining its buffered commands.
pub enum AfterDrain {
    KeepOpen,
    Close,
}

/// kevy's command set, plugged into the `kevy-rt` runtime. Stateless — the
/// keyspace lives in each shard's `Store`, so this is a zero-sized clone target.
#[derive(Clone, Copy, Default)]
pub struct KevyCommands;

impl Commands for KevyCommands {
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route {
        let Some(name) = args.first() else {
            return Route::Local;
        };
        let mut buf = [0u8; 32];
        match upper_verb(name, &mut buf) {
            b"HELLO" => Route::Hello,
            b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG"
            | b"INFO" | b"CLUSTER" | b"DEBUG" | b"WAIT" | b"SHUTDOWN"
            | b"CLIENT" | b"SELECT" => Route::Local,
            b"DBSIZE" => Route::Dbsize,
            b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
            b"SAVE" | b"BGSAVE" => Route::Save,
            b"BGREWRITEAOF" => Route::RewriteAof,
            // Cross-shard multi-key (malformed arity falls back to Local so the
            // dispatch stub returns the arity error).
            b"MSET" if args.len() >= 3 && !args.len().is_multiple_of(2) => Route::MSet,
            b"MGET" if args.len() >= 2 => Route::MGet,
            b"SINTER" if args.len() >= 2 => Route::SInter,
            b"SUNION" if args.len() >= 2 => Route::SUnion,
            b"SDIFF" if args.len() >= 2 => Route::SDiff,
            b"KEYS" if args.len() == 2 => Route::Keys(Some(args[1].to_vec())),
            b"SCAN" if args.len() >= 2 => Route::Scan(scan_pattern(args)),
            b"RANDOMKEY" if args.len() == 1 => Route::RandomKey,
            b"SUBSCRIBE" if args.len() >= 2 => Route::Subscribe,
            b"UNSUBSCRIBE" => Route::Unsubscribe, // no args = unsubscribe all
            b"PSUBSCRIBE" if args.len() >= 2 => Route::Psubscribe,
            b"PUNSUBSCRIBE" => Route::Punsubscribe, // no args = punsubscribe all
            b"PUBLISH" if args.len() == 3 => Route::Publish,
            b"WATCH" if args.len() >= 2 => Route::Watch,
            b"UNWATCH" => Route::Unwatch,
            // DEL/EXISTS are single-key (fast path) unless given multiple keys.
            b"DEL" => {
                if args.len() == 2 {
                    Route::Single(1)
                } else {
                    Route::DelKeys
                }
            }
            b"EXISTS" => {
                if args.len() == 2 {
                    Route::Single(1)
                } else {
                    Route::ExistsKeys
                }
            }
            // All remaining commands act on a single key at args[1].
            _ => {
                if args.len() >= 2 {
                    Route::Single(1)
                } else {
                    Route::Local // malformed; dispatch will return the error
                }
            }
        }
    }

    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        dispatch(store, args)
    }

    fn dispatch_into<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A, out: &mut Vec<u8>) {
        dispatch::dispatch_into(store, args, out)
    }

    fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        args.first()
            .is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
    }

    fn on_shard_init(&self, store: &mut Store) {
        // Snapshot the process-wide config and apply its `[memory]` section
        // to this shard. Reading `config_global::get()` returns
        // `Config::default()` (maxmemory=0) when running outside `serve` —
        // e.g. tests / embedded — so the call is harmlessly a no-op there.
        let cfg = config_global::get();
        store.set_max_memory(
            cfg.memory.maxmemory,
            map_eviction_policy(cfg.memory.maxmemory_policy),
        );
    }

    fn shard_tick_interval_ms(&self) -> u64 {
        // hz=0 disables the active reaper (lazy expiry still runs); else
        // every `1000/hz` ms — capped at 10 s so a misconfig can't park the
        // reactor's tick check loop forever.
        let cfg = config_global::get();
        let hz = cfg.expiry.hz;
        if hz == 0 {
            0
        } else {
            (1000 / hz as u64).clamp(1, 10_000)
        }
    }

    fn on_shard_tick(&self, store: &mut Store) {
        // Run Redis's `activeExpireCycle` per shard. `sample` controls the
        // batch size; up to 16 rounds per tick is well below Redis's 25 %
        // CPU budget at the default 10 Hz cadence. Cheap when no TTL'd
        // keys exist (a single map-emptiness check + bucket walk).
        let cfg = config_global::get();
        let samples = cfg.expiry.sample as usize;
        store.tick_expire(samples, 16);
        // Re-apply maxmemory + eviction policy in case `CONFIG SET` has
        // swapped the global since the previous tick. `store.set_max_memory`
        // is idempotent and cheap (compares + assigns two scalars + may
        // recompute soft-limit accounting); paying it every 100 ms is well
        // below the noise floor of any benchmark.
        store.set_max_memory(
            cfg.memory.maxmemory,
            map_eviction_policy(cfg.memory.maxmemory_policy),
        );
    }

    fn live_runtime_config(&self) -> kevy_rt::LiveRuntimeConfig {
        // Per-tick (every 100 ms by default) re-read of the process-wide
        // config. When the embedder hasn't called `config_init` (tests,
        // hand-rolled `Runtime`s in examples), return all-None so the
        // builder's explicit `with_appendfsync` / `with_auto_aof_rewrite`
        // choices aren't silently clobbered by `Config::default()` values.
        // Once `config_init` has run, every field is wrapped in `Some` so
        // the shard re-applies CONFIG SET changes within one tick.
        if !config_global::is_initialised() {
            return kevy_rt::LiveRuntimeConfig::default();
        }
        let cfg = config_global::get();
        let hz = cfg.expiry.hz;
        let tick_ms = if hz == 0 {
            Some(0)
        } else {
            Some((1000u64 / hz as u64).clamp(1, 10_000))
        };
        kevy_rt::LiveRuntimeConfig {
            appendfsync: Some(map_appendfsync(cfg.persistence.appendfsync)),
            auto_aof_rewrite_pct: Some(cfg.persistence.auto_aof_rewrite_percentage),
            auto_aof_rewrite_min_size: Some(cfg.persistence.auto_aof_rewrite_min_size),
            tick_interval_ms: tick_ms,
        }
    }

    fn hello_reply<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        current_proto: RespVersion,
    ) -> (RespVersion, Vec<u8>) {
        kevy_hello_reply(args, current_proto)
    }

    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        let Some(name) = args.first() else {
            return false;
        };
        let mut buf = [0u8; 32];
        cmd::is_write_verb(upper_verb(name, &mut buf))
    }

    fn txn_kind<A: ArgvView + ?Sized>(&self, args: &A) -> TxnKind {
        let Some(name) = args.first() else {
            return TxnKind::Other;
        };
        let mut buf = [0u8; 32];
        match upper_verb(name, &mut buf) {
            b"MULTI" => TxnKind::Multi,
            b"EXEC" => TxnKind::Exec,
            b"DISCARD" => TxnKind::Discard,
            b"WATCH" => TxnKind::Watch,
            _ => TxnKind::Other,
        }
    }

    /// One-pass verb resolution — the reactor calls this once per cmd and
    /// reads back txn_kind / route / is_quit / is_write without re-scanning
    /// the verb. This is `kevy-rt`'s primary hot-path optimization: every
    /// match arm uses the same `upper` buffer.
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        let Some(name) = args.first() else {
            return ResolvedCmd {
                txn_kind: TxnKind::Other,
                route: Route::Local,
                is_quit: false,
                is_write: false,
            };
        };
        let mut buf = [0u8; 32];
        let upper = upper_verb(name, &mut buf);

        let txn_kind = match upper {
            b"MULTI" => TxnKind::Multi,
            b"EXEC" => TxnKind::Exec,
            b"DISCARD" => TxnKind::Discard,
            b"WATCH" => TxnKind::Watch,
            _ => TxnKind::Other,
        };

        let is_quit = upper == b"QUIT";

        let is_write = cmd::is_write_verb(upper);

        let route = match upper {
            b"HELLO" => Route::Hello,
            b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG"
            | b"INFO" | b"CLUSTER" | b"DEBUG" | b"WAIT" | b"SHUTDOWN"
            | b"CLIENT" | b"SELECT" => Route::Local,
            b"DBSIZE" => Route::Dbsize,
            b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
            b"SAVE" | b"BGSAVE" => Route::Save,
            b"BGREWRITEAOF" => Route::RewriteAof,
            b"MSET" if args.len() >= 3 && !args.len().is_multiple_of(2) => Route::MSet,
            b"MGET" if args.len() >= 2 => Route::MGet,
            b"SINTER" if args.len() >= 2 => Route::SInter,
            b"SUNION" if args.len() >= 2 => Route::SUnion,
            b"SDIFF" if args.len() >= 2 => Route::SDiff,
            b"KEYS" if args.len() == 2 => Route::Keys(Some(args[1].to_vec())),
            b"SCAN" if args.len() >= 2 => Route::Scan(scan_pattern(args)),
            b"RANDOMKEY" if args.len() == 1 => Route::RandomKey,
            b"SUBSCRIBE" if args.len() >= 2 => Route::Subscribe,
            b"UNSUBSCRIBE" => Route::Unsubscribe,
            b"PSUBSCRIBE" if args.len() >= 2 => Route::Psubscribe,
            b"PUNSUBSCRIBE" => Route::Punsubscribe,
            b"PUBLISH" if args.len() == 3 => Route::Publish,
            b"WATCH" if args.len() >= 2 => Route::Watch,
            b"UNWATCH" => Route::Unwatch,
            b"DEL" => {
                if args.len() == 2 {
                    Route::Single(1)
                } else {
                    Route::DelKeys
                }
            }
            b"EXISTS" => {
                if args.len() == 2 {
                    Route::Single(1)
                } else {
                    Route::ExistsKeys
                }
            }
            _ => {
                if args.len() >= 2 {
                    Route::Single(1)
                } else {
                    Route::Local
                }
            }
        };

        ResolvedCmd {
            txn_kind,
            route,
            is_quit,
            is_write,
        }
    }
}

/// Parse the optional `HELLO [protover [AUTH user pass] [SETNAME name]]`
/// arguments, validate the proto switch request, and emit the right
/// reply shape (RESP2 array-of-pairs or RESP3 Map) for the resulting
/// proto. Returns `(new_proto, reply_bytes)` — the runtime applies
/// `new_proto` to the conn BEFORE folding the reply so a `HELLO 3` ack
/// itself goes out as a RESP3 Map.
///
/// Unsupported proto requests (`HELLO 4`, `HELLO 1`) leave the proto
/// unchanged + emit `-NOPROTO unsupported protocol version`. AUTH /
/// SETNAME tails are currently parsed-and-ignored (kevy has no AUTH
/// and CLIENT SETNAME is already a stub — see scope-decisions.md).
fn kevy_hello_reply<A: ArgvView + ?Sized>(
    args: &A,
    current_proto: RespVersion,
) -> (RespVersion, Vec<u8>) {
    let new_proto = match args.get(1) {
        None => current_proto,
        Some(b"2") => RespVersion::V2,
        Some(b"3") => RespVersion::V3,
        Some(_) => {
            let mut out = Vec::new();
            encode_error(
                &mut out,
                "NOPROTO unsupported protocol version (kevy supports 2 and 3)",
            );
            return (current_proto, out);
        }
    };
    let mut out = Vec::new();
    encode_hello_reply(&mut out, new_proto);
    (new_proto, out)
}

/// Emit the HELLO ack body shaped per `proto`. RESP2: flat
/// `*14\r\n...` array-of-pairs (kept identical to the pre-v1.4 wire
/// for backward-compat). RESP3: `%7\r\n...` Map with the same 7 fields.
fn encode_hello_reply(out: &mut Vec<u8>, proto: RespVersion) {
    let proto_int = match proto {
        RespVersion::V2 => 2,
        RespVersion::V3 => 3,
    };
    match proto {
        RespVersion::V2 => encode_array_len(out, 14),
        RespVersion::V3 => encode_map_header(out, 7),
    }
    encode_bulk(out, b"server");
    encode_bulk(out, b"kevy");
    encode_bulk(out, b"version");
    encode_bulk(out, env!("CARGO_PKG_VERSION").as_bytes());
    encode_bulk(out, b"proto");
    encode_integer(out, proto_int);
    encode_bulk(out, b"id");
    encode_integer(out, 0);
    encode_bulk(out, b"mode");
    encode_bulk(out, b"standalone");
    encode_bulk(out, b"role");
    encode_bulk(out, b"master");
    encode_bulk(out, b"modules");
    encode_array_len(out, 0);
}

/// Translate a `kevy_config::EvictionPolicy` (the user-facing TOML enum) into
/// the `kevy_store::EvictionPolicy` mirror. The mapping is one-to-one — the
/// two enums exist as a dependency-direction trick (kevy-store stays a leaf
/// crate; kevy-config depends on nothing kevy-store does).
fn map_eviction_policy(p: kevy_config::EvictionPolicy) -> kevy_store::EvictionPolicy {
    use kevy_config::EvictionPolicy as C;
    use kevy_store::EvictionPolicy as S;
    match p {
        C::NoEviction => S::NoEviction,
        C::AllKeysLru => S::AllKeysLru,
        C::AllKeysLfu => S::AllKeysLfu,
        C::AllKeysRandom => S::AllKeysRandom,
        C::VolatileLru => S::VolatileLru,
        C::VolatileLfu => S::VolatileLfu,
        C::VolatileRandom => S::VolatileRandom,
        C::VolatileTtl => S::VolatileTtl,
    }
}

/// Run the thread-per-core server forever: `nshards` shards on `ip:port`,
/// snapshotting to / restoring from `data_dir`, with the AOF on/off.
///
/// Reads `cfg.persistence.appendfsync` from the process-wide config (set by
/// `config_init`) to pick the AOF fsync policy. Defaults to `EverySec`
/// when no config is installed (matches the Wave 1 behaviour).
pub fn serve(ip: [u8; 4], port: u16, nshards: usize, data_dir: PathBuf, enable_aof: bool) -> ! {
    let cfg = config_global::get();
    let fsync = map_appendfsync(cfg.persistence.appendfsync);
    let runtime = Runtime::new(ip, port, nshards, KevyCommands)
        .with_data_dir(data_dir)
        .with_aof(enable_aof)
        .with_appendfsync(fsync)
        .with_auto_aof_rewrite(
            cfg.persistence.auto_aof_rewrite_percentage,
            cfg.persistence.auto_aof_rewrite_min_size,
        );
    let stop = Arc::new(AtomicBool::new(false));
    if let Err(e) = runtime.run(stop) {
        eprintln!("kevy: runtime error: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

/// Translate a `kevy_config::AppendFsync` (TOML enum) into the
/// `kevy_persist::Fsync` mirror. Same dependency-direction story as
/// [`map_eviction_policy`].
fn map_appendfsync(p: kevy_config::AppendFsync) -> kevy_persist::Fsync {
    use kevy_config::AppendFsync as C;
    use kevy_persist::Fsync as P;
    match p {
        C::Always => P::Always,
        C::EverySec => P::EverySec,
        C::No => P::No,
    }
}

/// Parse and dispatch every complete command in `input`, appending replies to
/// `output`. Consumes parsed bytes; leaves a trailing partial frame. Returns
/// `Close` after a `QUIT` or a protocol error (whose reply is already appended).
pub fn drain_commands(store: &mut Store, input: &mut Vec<u8>, output: &mut Vec<u8>) -> AfterDrain {
    loop {
        match parse_command(input) {
            Ok(Some((args, consumed))) => {
                let reply = dispatch(store, &args);
                output.extend_from_slice(&reply);
                input.drain(..consumed);
                if args
                    .first()
                    .is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
                {
                    return AfterDrain::Close;
                }
            }
            Ok(None) => return AfterDrain::KeepOpen,
            Err(_) => {
                encode_error(output, "ERR Protocol error");
                return AfterDrain::Close;
            }
        }
    }
}

/// Blocking single-connection handler. Shares command logic with the reactor;
/// retained for tests and simple uses.
pub fn handle_conn(conn: &Socket, store: &mut Store) -> io::Result<()> {
    let mut input: Vec<u8> = Vec::with_capacity(4096);
    let mut output: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let after = drain_commands(store, &mut input, &mut output);
        if !output.is_empty() {
            conn.write_all(&output)?;
            output.clear();
        }
        if matches!(after, AfterDrain::Close) {
            return Ok(());
        }
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        input.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests;
