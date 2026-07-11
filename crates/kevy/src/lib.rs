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
//! use kevy::{Argv, KevyCommands, KeyspaceStore};
//!
//! let kevy = KevyCommands::new();
//! let mut store = KeyspaceStore::new();
//! let cmd = |parts: &[&[u8]]| Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
//! assert_eq!(kevy.dispatch(&mut store, &cmd(&[b"SET", b"k", b"v"])), b"+OK\r\n");
//! assert_eq!(kevy.dispatch(&mut store, &cmd(&[b"GET", b"k"])), b"$1\r\nv\r\n");
//! assert_eq!(kevy.dispatch(&mut store, &cmd(&[b"INCR", b"n"])), b":1\r\n");
//! ```
//!
//! To run the full server: [`serve`]`(config)`.
#![forbid(unsafe_code)]

use kevy_resp::{encode_error, parse_command};
use kevy_rt::Runtime;
use kevy_store::Store;
use kevy_sys::Socket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod cmd;
mod cmd_class;
mod cmd_zadd;
mod cmd_block;
mod metrics_http;
mod cmd_block_serve;
mod cmd_data;
mod cmd_hash_ttl;
mod cmd_index;
mod cmd_digest;
mod cmd_view;
mod cmd_view_reduce;
mod cmd_index_query;
mod cmd_index_reduce;
mod index_runtime;
mod view_runtime;
mod cmd_hello;
mod cmd_lua;
mod cmd_repl;
mod cmd_resolve;
mod commands;
mod replication;
mod dispatch;
mod dispatch_collections;
mod dispatch_collections_v127;
mod dispatch_resp3;
mod dispatch_geo;
mod dispatch_stream;
mod elect_persist;
mod ops;
mod replica_runner;
mod state;
pub mod verb_meta;
mod cmd_command;
mod cmd_failover;

pub use kevy_rt::Argv;
pub use kevy_store::Store as KeyspaceStore;
pub use state::{KevyCommands, RuntimeState};

/// What to do with a connection after draining its buffered commands.
pub enum AfterDrain {
    KeepOpen,
    Close,
}


/// Translate a `kevy_config::EvictionPolicy` (the user-facing TOML enum) into
/// the `kevy_store::EvictionPolicy` mirror. The mapping is one-to-one — the
/// two enums exist as a dependency-direction trick (kevy-store stays a leaf
/// crate; kevy-config depends on nothing kevy-store does).
pub(crate) fn map_eviction_policy(p: kevy_config::EvictionPolicy) -> kevy_store::EvictionPolicy {
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

/// **v1.39** — signal flag flipped by the SIGTERM / SIGINT handler.
/// Async-signal-safe; AtomicBool::store is signal-safe per the C
/// memory model.
#[cfg(unix)]
static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);
/// Every live server's stop flag. Signal disposition is a PROCESS
/// property (the handler must be async-signal-safe, so it can only
/// flip the static above); this registry fans the process-level
/// signal out to every runtime instance, and registration resets a
/// leftover signal from a previous run so a second serve() in the
/// same process doesn't exit on arrival.
static STOP_FLAGS: std::sync::Mutex<Vec<std::sync::Weak<AtomicBool>>> = std::sync::Mutex::new(Vec::new());

/// **v1.39** — installed on first call to [`serve`]. Catches SIGTERM
/// (graceful shutdown) and SIGINT (Ctrl-C). Both flip the per-run
/// `stop` flag via a polling bridge thread.
#[cfg(unix)]
fn install_signal_handlers(stop: Arc<AtomicBool>) {
    extern "C" fn handler(_: std::ffi::c_int) {
        SIGNAL_RECEIVED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    kevy_sys::install_signal_handler(kevy_sys::SIGTERM, handler);
    kevy_sys::install_signal_handler(kevy_sys::SIGINT, handler);
    // v1.58 (closes v1.38.x finding): SIGXFSZ is raised when a write
    // would exceed RLIMIT_FSIZE. Default action is `Core` (kernel
    // dump). Installing a no-op handler absorbs the signal — the
    // failing write returns EFBIG to the AOF writer (logged and
    // ignored), kevy keeps serving reads and continues attempting
    // writes. One bad write does not bring down the whole server.
    extern "C" fn xfsz_noop(_: std::ffi::c_int) {}
    kevy_sys::install_signal_handler(kevy_sys::SIGXFSZ, xfsz_noop);
    // Register this run's stop flag and clear any signal left over
    // from an earlier run in this process. One polling bridge thread
    // fans the flag out to every registered runtime (SIGTERM means
    // "the whole process stops" — broadcast is the right semantic);
    // handlers themselves stay async-signal-safe.
    let mut flags = STOP_FLAGS.lock().expect("STOP_FLAGS poisoned");
    let first = flags.is_empty();
    SIGNAL_RECEIVED.store(false, std::sync::atomic::Ordering::SeqCst);
    flags.push(Arc::downgrade(&stop));
    drop(flags);
    if first {
        std::thread::spawn(|| loop {
            if SIGNAL_RECEIVED.load(std::sync::atomic::Ordering::SeqCst) {
                let flags = STOP_FLAGS.lock().expect("STOP_FLAGS poisoned");
                for f in flags.iter() {
                    if let Some(stop) = f.upgrade() {
                        stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
    }
}

#[cfg(not(unix))]
fn install_signal_handlers(_stop: Arc<AtomicBool>) {
    // No-op on non-Unix; production deployments are Unix anyway.
}

/// Run the thread-per-core server forever, entirely shaped by `cfg`:
/// `cfg.server.threads` shards on `cfg.server.bind:cfg.server.port`,
/// snapshotting to / restoring from `cfg.server.data_dir`, AOF per
/// `cfg.persistence.aof`. `threads = 0` (the auto sentinel) runs one
/// shard; the CLI resolves auto to `available_parallelism()` before
/// calling in.
pub fn serve(cfg: Arc<kevy_config::Config>) -> ! {
    let state = boot_state(&cfg);
    let runtime = build_runtime(&cfg, KevyCommands::with_state(Arc::clone(&state)));
    // Spawn the kevy-elect control plane when the operator configured
    // `[cluster] peers = "..."` + `node_id`. Opt-in; empty peers
    // leaves the subsystem dormant.
    state.election.maybe_start(&cfg, &state.replication);
    let stop = Arc::new(AtomicBool::new(false));
    // v1.39 — install SIGTERM + SIGINT handlers that flip `stop`,
    // triggering the runtime's existing drain path (fsync AOF, close
    // listeners, exit 0). std-only: raw `signal(2)` + a poller thread
    // that bridges the signal-safe static into the per-run `Arc`.
    install_signal_handlers(Arc::clone(&stop));
    // v1.41 — Prometheus /metrics endpoint. No-op when port = 0.
    metrics_http::spawn_if_enabled(&state);
    // Replica runners (if any) live in `state.replication` — they
    // are started by `replication::apply` for the startup
    // `role = "replica"` path and by `REPLICAOF` at runtime
    // (T1.29.5). On exit the runners are dropped with the state; the
    // `Drop` impl signals stop + joins each runner thread, so the
    // process exits cleanly with no orphan TCP fds.
    let run_result = runtime.run(stop);
    // Stop kevy-elect after the runtime exits so the control plane
    // doesn't outlive the data plane.
    state.election.shutdown();
    if let Err(e) = run_result {
        eprintln!("kevy: runtime error: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

/// Build the [`RuntimeState`] for one server boot: create the data
/// dir (a precondition of AOF, index catalogs, elect.meta and
/// replication state — fail here with a named error, not later with
/// a bare ENOENT), validate `[cluster] scopes`, and load the index /
/// view sidecars.
fn boot_state(cfg: &Arc<kevy_config::Config>) -> Arc<RuntimeState> {
    let data_dir = cfg.server.data_dir.clone();
    let nshards = cfg.server.threads.max(1);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("kevy: cannot create data dir {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    let state = match RuntimeState::new(Arc::clone(cfg), data_dir, nshards) {
        Ok(s) => Arc::new(s),
        Err(msg) => {
            eprintln!("kevy: bad [cluster] scopes config: {msg}");
            std::process::exit(1);
        }
    };
    cmd_index::boot(&state);
    cmd_view::boot(&state);
    state
}

/// Assemble the configured [`Runtime`]: the builder chain plus the
/// cluster / feed / UDS opt-in branches and the replication wiring.
fn build_runtime(cfg: &kevy_config::Config, commands: KevyCommands) -> Runtime<KevyCommands> {
    let state = Arc::clone(commands.state());
    let nshards = state.nshards();
    let fsync = map_appendfsync(cfg.persistence.appendfsync);
    let mut runtime = Runtime::builder(commands)
        .bind(cfg.server.bind, cfg.server.port)
        .shards(nshards)
        .with_data_dir(cfg.server.data_dir.clone())
        .with_accept_shards(cfg.server.accept_shards)
        .with_max_clients(cfg.server.max_clients)
        .with_aof(cfg.persistence.aof)
        .with_appendfsync(fsync)
        .with_auto_aof_rewrite(
            cfg.persistence.auto_aof_rewrite_percentage,
            cfg.persistence.auto_aof_rewrite_min_size,
        )
        .with_advanced(
            cfg.advanced.spin_limit,
            cfg.advanced.park_timeout_ms,
            cfg.advanced.tick_check_every,
            cfg.advanced.ring_capacity,
        )
        .with_slowlog(cfg.slowlog.slower_than_micros, cfg.slowlog.max_len);
    if cfg.cluster.enabled {
        runtime = runtime.with_cluster(cluster_port_base(cfg));
    }
    if cfg.feed.enabled {
        runtime = runtime.with_feed(true, cfg.feed.feed_buffer_size);
    }
    // v1.25 UDS: opt-in via `KEVY_UNIX_SOCKET=/path/to/sock` env var. Lets
    // local clients (and benches) skip TCP loopback overhead — fair
    // comparison against valkey/redis's `unixsocket` config.
    if let Ok(path) = std::env::var("KEVY_UNIX_SOCKET")
        && !path.is_empty() {
            runtime = runtime.with_unix_socket(PathBuf::from(path));
        }
    replication::apply(runtime, cfg, &state)
}

/// Resolved first cluster port: `[cluster].port_base`, or `server.port + 1`
/// when left at the `0` default. Shard `i` listens at this + `i`.
pub(crate) fn cluster_port_base(cfg: &kevy_config::Config) -> u16 {
    match cfg.cluster.port_base {
        // saturating: port 65535 would overflow; Runtime::run then rejects
        // the (base, nshards) range loudly rather than wrapping a listener.
        0 => cfg.server.port.saturating_add(1),
        base => base,
    }
}

/// Translate a `kevy_config::AppendFsync` (TOML enum) into the
/// `kevy_persist::Fsync` mirror. Same dependency-direction story as
/// [`map_eviction_policy`].
pub(crate) fn map_appendfsync(p: kevy_config::AppendFsync) -> kevy_persist::Fsync {
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
pub fn drain_commands(
    kevy: &KevyCommands,
    store: &mut Store,
    input: &mut Vec<u8>,
    output: &mut Vec<u8>,
) -> AfterDrain {
    loop {
        match parse_command(input) {
            Ok(Some((args, consumed))) => {
                let reply = kevy.dispatch(store, &args);
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
pub fn handle_conn(kevy: &KevyCommands, conn: &Socket, store: &mut Store) -> io::Result<()> {
    let mut input: Vec<u8> = Vec::with_capacity(4096);
    let mut output: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let after = drain_commands(kevy, store, &mut input, &mut output);
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
#[cfg(test)]
mod tests_op_table;
#[cfg(test)]
mod tests_verb_meta;
