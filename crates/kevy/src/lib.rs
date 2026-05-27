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

use kevy_resp::{encode_error, parse_command};
use kevy_rt::{Commands, ResolvedCmd, Route, Runtime, TxnKind};
use kevy_store::Store;
use kevy_sys::Socket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod cmd;
mod config_global;
mod dispatch;
mod ops;

pub use config_global::init as config_init;
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
    fn route(&self, args: &Argv) -> Route {
        let Some(name) = args.first() else {
            return Route::Local;
        };
        let mut buf = [0u8; 32];
        match upper_verb(name, &mut buf) {
            b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG" | b"HELLO"
            | b"INFO" | b"CLUSTER" | b"DEBUG" | b"WAIT" | b"SHUTDOWN" => Route::Local,
            b"DBSIZE" => Route::Dbsize,
            b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
            b"SAVE" | b"BGSAVE" => Route::Save,
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
            b"PUBLISH" if args.len() == 3 => Route::Publish,
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

    fn dispatch(&self, store: &mut Store, args: &Argv) -> Vec<u8> {
        dispatch(store, args)
    }

    fn dispatch_into(&self, store: &mut Store, args: &Argv, out: &mut Vec<u8>) {
        dispatch::dispatch_into(store, args, out)
    }

    fn is_quit(&self, args: &Argv) -> bool {
        args.first()
            .is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
    }

    fn is_write(&self, args: &Argv) -> bool {
        let Some(name) = args.first() else {
            return false;
        };
        let mut buf = [0u8; 32];
        matches!(
            upper_verb(name, &mut buf),
            b"SET"
                | b"SETNX"
                | b"SETEX"
                | b"PSETEX"
                | b"GETSET"
                | b"GETDEL"
                | b"INCRBYFLOAT"
                | b"DEL"
                | b"INCR"
                | b"DECR"
                | b"INCRBY"
                | b"DECRBY"
                | b"APPEND"
                | b"EXPIRE"
                | b"PEXPIRE"
                | b"PERSIST"
                | b"FLUSHDB"
                | b"FLUSHALL"
                | b"HSET"
                | b"HSETNX"
                | b"HDEL"
                | b"HINCRBY"
                | b"LPUSH"
                | b"RPUSH"
                | b"LPOP"
                | b"RPOP"
                | b"LSET"
                | b"LREM"
                | b"LTRIM"
                | b"SADD"
                | b"SREM"
                | b"SPOP"
                | b"ZADD"
                | b"ZREM"
                | b"ZINCRBY"
                | b"MSET"
        )
    }

    fn txn_kind(&self, args: &Argv) -> TxnKind {
        let Some(name) = args.first() else {
            return TxnKind::Other;
        };
        let mut buf = [0u8; 32];
        match upper_verb(name, &mut buf) {
            b"MULTI" => TxnKind::Multi,
            b"EXEC" => TxnKind::Exec,
            b"DISCARD" => TxnKind::Discard,
            _ => TxnKind::Other,
        }
    }

    /// One-pass verb resolution — the reactor calls this once per cmd and
    /// reads back txn_kind / route / is_quit / is_write without re-scanning
    /// the verb. This is `kevy-rt`'s primary hot-path optimization: every
    /// match arm uses the same `upper` buffer.
    fn resolve(&self, args: &Argv) -> ResolvedCmd {
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
            _ => TxnKind::Other,
        };

        let is_quit = upper == b"QUIT";

        let is_write = matches!(
            upper,
            b"SET"
                | b"SETNX"
                | b"SETEX"
                | b"PSETEX"
                | b"GETSET"
                | b"GETDEL"
                | b"INCRBYFLOAT"
                | b"DEL"
                | b"INCR"
                | b"DECR"
                | b"INCRBY"
                | b"DECRBY"
                | b"APPEND"
                | b"EXPIRE"
                | b"PEXPIRE"
                | b"PERSIST"
                | b"FLUSHDB"
                | b"FLUSHALL"
                | b"HSET"
                | b"HSETNX"
                | b"HDEL"
                | b"HINCRBY"
                | b"LPUSH"
                | b"RPUSH"
                | b"LPOP"
                | b"RPOP"
                | b"LSET"
                | b"LREM"
                | b"LTRIM"
                | b"SADD"
                | b"SREM"
                | b"SPOP"
                | b"ZADD"
                | b"ZREM"
                | b"ZINCRBY"
                | b"MSET"
        );

        let route = match upper {
            b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG" | b"HELLO"
            | b"INFO" | b"CLUSTER" | b"DEBUG" | b"WAIT" | b"SHUTDOWN" => Route::Local,
            b"DBSIZE" => Route::Dbsize,
            b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
            b"SAVE" | b"BGSAVE" => Route::Save,
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
            b"PUBLISH" if args.len() == 3 => Route::Publish,
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

/// Run the thread-per-core server forever: `nshards` shards on `ip:port`,
/// snapshotting to / restoring from `data_dir`, with the AOF on/off.
pub fn serve(ip: [u8; 4], port: u16, nshards: usize, data_dir: PathBuf, enable_aof: bool) -> ! {
    let runtime = Runtime::new(ip, port, nshards, KevyCommands)
        .with_data_dir(data_dir)
        .with_aof(enable_aof);
    let stop = Arc::new(AtomicBool::new(false));
    if let Err(e) = runtime.run(stop) {
        eprintln!("kevy: runtime error: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
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
