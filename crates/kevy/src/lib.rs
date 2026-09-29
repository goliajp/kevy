//! kevy — a single-machine, Redis-compatible key–value server.
//!
//! This crate is the server: it supplies the command *semantics* — routing
//! and execution, both in [`KevyCommands`] — and wires them to the
//! [kevy-rt] shared-nothing thread-per-core runtime via [`serve`]. The command
//! logic is also reachable directly (one keyspace, no I/O) through
//! [`KevyCommands::dispatch`], which is handy for embedding or testing. Built
//! from a small stack of zero-dependency crates: [kevy-sys], [kevy-resp],
//! [kevy-store], [kevy-rt], [kevy-persist].
//!
//! [kevy-rt]: https://crates.io/crates/kevy-rt
//! [kevy-sys]: https://crates.io/crates/kevy-sys
//! [kevy-resp]: https://crates.io/crates/kevy-resp
//! [kevy-store]: https://crates.io/crates/kevy-store
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
mod cmd_block;
mod cmd_block_serve;
mod cmd_class;
mod cmd_command;
mod cmd_describe;
mod cmd_digest;
mod cmd_failover;
mod cmd_global_sample;
mod cmd_hello;
mod cmd_index;
mod cmd_index_advise;
mod cmd_index_install;
mod cmd_index_query;
mod cmd_index_reduce;
mod cmd_lua;
mod cmd_repl;
mod cmd_resolve;
mod cmd_table;
mod cmd_table_global;
mod cmd_table_verify;
mod cmd_view;
mod cmd_view_reduce;
mod commands;
mod commands_ext;
mod dispatch;
mod dispatch_replay;
mod dispatch_resp3;
mod elect_persist;
mod geo_store;
mod index_runtime;
mod key_walk;
mod mem_guard;
mod metrics_http;
mod ops;
mod replica_runner;
mod replica_runner_events;
mod replica_runner_routed;
mod replica_trace;
mod replication;
pub mod secure;
mod secure_front;
mod state;
mod table_runtime;
mod tier_read;
mod tiering_boot;
pub mod verb_meta;
mod view_runtime;

pub use kevy_rt::Argv;
pub use kevy_scope::OwnershipError;
pub use kevy_store::Store as KeyspaceStore;
pub use state::{KevyCommands, RuntimeState};

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub(crate) use tiering_boot::{resolve_tier_budget, wire_tiering};

/// What to do with a connection after draining its buffered commands.
///
/// ```
/// use kevy::{AfterDrain, KevyCommands, KeyspaceStore, drain_commands};
/// let (kevy, mut store, mut out) = (KevyCommands::new(), KeyspaceStore::new(), Vec::new());
/// let mut input = b"*1\r\n$4\r\nPING\r\n".to_vec();
/// let after = drain_commands(&kevy, &mut store, &mut input, &mut out);
/// assert_eq!((after, &out[..]), (AfterDrain::KeepOpen, &b"+PONG\r\n"[..]));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AfterDrain {
    /// Keep serving this connection — the ordinary outcome.
    ///
    /// ```
    /// use kevy::{AfterDrain, KevyCommands, KeyspaceStore, drain_commands};
    /// let (kevy, mut store, mut out) = (KevyCommands::new(), KeyspaceStore::new(), Vec::new());
    /// // half a frame: nothing to answer yet, wait for the rest
    /// let mut input = b"*1\r\n$4\r\nPI".to_vec();
    /// assert_eq!(drain_commands(&kevy, &mut store, &mut input, &mut out), AfterDrain::KeepOpen);
    /// assert!(out.is_empty());
    /// ```
    KeepOpen,
    /// Close it: the client sent QUIT, or the connection is being shut
    /// down for a reason the drain already replied about. The reply is
    /// written before the close, so this is not an abort.
    ///
    /// ```
    /// use kevy::{AfterDrain, KevyCommands, KeyspaceStore, drain_commands};
    /// let (kevy, mut store, mut out) = (KevyCommands::new(), KeyspaceStore::new(), Vec::new());
    /// let mut input = b"*1\r\n$4\r\nQUIT\r\n".to_vec();
    /// assert_eq!(drain_commands(&kevy, &mut store, &mut input, &mut out), AfterDrain::Close);
    /// assert_eq!(out, b"+OK\r\n", "the reply goes out before the close");
    /// ```
    Close,
}

/// Signal flag flipped by the SIGTERM / SIGINT handler.
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
static STOP_FLAGS: std::sync::Mutex<Vec<std::sync::Weak<AtomicBool>>> =
    std::sync::Mutex::new(Vec::new());

/// Installed on first call to [`serve`]. Catches SIGTERM
/// (graceful shutdown) and SIGINT (Ctrl-C). Both flip the per-run
/// `stop` flag via a polling bridge thread.
#[cfg(unix)]
fn install_signal_handlers(stop: Arc<AtomicBool>) {
    // NO-UNWIND: signal handler: one atomic store
    extern "C" fn handler(_: std::ffi::c_int) {
        SIGNAL_RECEIVED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    kevy_sys::install_signal_handler(kevy_sys::SIGTERM, handler);
    kevy_sys::install_signal_handler(kevy_sys::SIGINT, handler);
    // SIGXFSZ is raised when a write
    // would exceed RLIMIT_FSIZE. Default action is `Core` (kernel
    // dump). Installing a no-op handler absorbs the signal — the
    // failing write returns EFBIG to the AOF writer (logged and
    // ignored), kevy keeps serving reads and continues attempting
    // writes. One bad write does not bring down the whole server.
    // NO-UNWIND: signal handler with an empty body
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
        std::thread::spawn(|| {
            loop {
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
            }
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
///
/// This one is compiled but not run: `serve` never returns, it serves until
/// SIGTERM / SIGINT / SHUTDOWN and then exits the process.
///
/// ```no_run
/// let mut cfg = kevy_config::Config::default();
/// cfg.server.port = 6004;
/// cfg.server.threads = 2;
/// kevy::serve(std::sync::Arc::new(cfg));
/// ```
pub fn serve(cfg: Arc<kevy_config::Config>) -> ! {
    // a secure link without its keys refuses to start, never falls back to plaintext
    let link_key = secure::link_keypair(&cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let state = boot_state(&cfg);
    if cfg.replication.secure
        && let Some(key) = &link_key
    {
        state.replication.set_links(secure::ReplLinks::from_config(&cfg, key));
    }
    let runtime = build_runtime(&cfg, KevyCommands::with_state(Arc::clone(&state)));
    let runtime = secure_front::start(&cfg, link_key.as_ref(), runtime);
    // Spawn the kevy-elect control plane when the operator configured
    // `[cluster] peers = "..."` + `node_id`. Opt-in; empty peers
    // leaves the subsystem dormant.
    state.election.maybe_start(&cfg, &state.replication, link_key.as_ref());
    let stop = Arc::new(AtomicBool::new(false));
    // Install SIGTERM + SIGINT handlers that flip `stop`,
    // triggering the runtime's existing drain path (fsync AOF, close
    // listeners, exit 0). std-only: raw `signal(2)` + a poller thread
    // that bridges the signal-safe static into the per-run `Arc`.
    install_signal_handlers(Arc::clone(&stop));
    // The SHUTDOWN command trips the same flag (plus an optional
    // final-snapshot request for `SHUTDOWN SAVE`).
    state.register_stop_flag(Arc::clone(&stop));
    // Prometheus /metrics endpoint. No-op when port = 0.
    metrics_http::spawn_if_enabled(&state);
    mem_guard::spawn_if_tiered(&state);
    // Replica runners (if any) live in `state.replication` — they
    // are started by `replication::apply` for the startup
    // `role = "replica"` path and by `REPLICAOF` at runtime.
    // On exit the runners are dropped with the state; the
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
        Err(e) => {
            eprintln!("kevy: bad [cluster] scopes config: {e}");
            std::process::exit(1);
        }
    };
    cmd_index::boot(&state);
    cmd_view::boot(&state);
    cmd_table::boot(&state);
    state
}

/// Assemble the configured [`Runtime`]: the builder chain plus the
/// cluster / feed / UDS opt-in branches and the replication wiring.
fn build_runtime(cfg: &kevy_config::Config, commands: KevyCommands) -> Runtime<KevyCommands> {
    let state = Arc::clone(commands.state());
    let nshards = state.nshards();
    let fsync = cfg.persistence.appendfsync;
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
        .with_auto_rewrite_bytes(cfg.persistence.auto_aof_rewrite_bytes)
        .with_auto_rewrite_interval_secs(cfg.persistence.auto_aof_rewrite_interval_secs)
        // Boot-time only: replay happens before the first tick, so the
        // live-config push (which lands at that tick) is too late for it.
        .with_replay_mode(replay_mode(cfg))
        .with_advanced(
            cfg.advanced.spin_limit,
            cfg.advanced.park_timeout_ms,
            cfg.advanced.tick_check_every,
            cfg.advanced.ring_capacity,
        )
        // the config admits only powers of two up to 32768, all within u16
        .with_recv_buffers(cfg.advanced.recv_buffers as u16)
        .with_slowlog(cfg.slowlog.slower_than_micros, cfg.slowlog.max_len);
    if cfg.cluster.enabled {
        runtime = runtime
            .with_cluster(cluster_port_base(cfg))
            .with_cluster_announce(cfg.cluster.announce_ip, announce_port_base(cfg));
    }
    if cfg.feed.enabled {
        runtime = runtime.with_feed(true).with_feed_buffer_size(cfg.feed.feed_buffer_size);
    }
    runtime = wire_tiering(runtime, cfg);
    // UDS: opt-in via `KEVY_UNIX_SOCKET=/path/to/sock` env var. Lets
    // local clients (and benches) skip TCP loopback overhead — fair
    // comparison against valkey/redis's `unixsocket` config.
    if let Ok(path) = std::env::var("KEVY_UNIX_SOCKET")
        && !path.is_empty()
    {
        runtime = runtime.with_unix_socket(PathBuf::from(path));
    }
    replication::apply(runtime, cfg, &state)
}

/// `[cluster].announce_port_base`, or `None` when left at `0` so the
/// listening ports are advertised.
pub(crate) fn announce_port_base(cfg: &kevy_config::Config) -> Option<u16> {
    (cfg.cluster.announce_port_base != 0).then_some(cfg.cluster.announce_port_base)
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

/// Parse and dispatch every complete command in `input`, appending replies to
/// `output`. Consumes parsed bytes; leaves a trailing partial frame. Returns
/// `Close` after a `QUIT` or a protocol error (whose reply is already appended).
///
/// ```
/// use kevy::{AfterDrain, KevyCommands, KeyspaceStore, drain_commands};
/// let (kevy, mut store, mut out) = (KevyCommands::new(), KeyspaceStore::new(), Vec::new());
/// // two whole commands and the start of a third
/// let mut input = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n*1\r\n".to_vec();
/// assert_eq!(drain_commands(&kevy, &mut store, &mut input, &mut out), AfterDrain::KeepOpen);
/// assert_eq!(out, b"+OK\r\n$1\r\nv\r\n");
/// assert_eq!(input, b"*1\r\n", "the partial frame stays for the next read");
/// ```
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
                // This simple path has no AOF / replication recorder, so
                // nothing consumes a propagation override — drop anything
                // a nondeterministic verb (SPOP) set, per command, so it
                // can't linger on this thread.
                kevy_rt::propagation::discard_override();
                output.extend_from_slice(&reply);
                input.drain(..consumed);
                if args.first().is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT")) {
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
///
/// ```
/// use std::io::{Read, Write};
/// let listener = kevy_sys::Socket::tcp_listen([127, 0, 0, 1], 0, 1)?;
/// let port = listener.local_port()?;
/// let client = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
///     let mut c = std::net::TcpStream::connect(("127.0.0.1", port))?;
///     c.write_all(b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nQUIT\r\n")?;
///     let mut replies = Vec::new();
///     c.read_to_end(&mut replies)?;
///     Ok(replies)
/// });
/// let conn = listener.accept()?;
/// kevy::handle_conn(&kevy::KevyCommands::new(), &conn, &mut kevy::KeyspaceStore::new())?;
/// drop(conn);
/// let replies = client.join().map_err(|_| "client thread panicked")??;
/// assert_eq!(replies, b"+PONG\r\n+OK\r\n");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller. KevyCommands is
// Send and deliberately not Sync: each shard thread owns its own clone,
// with per-shard state in cells.
const _: () = {
    const fn send<T: Send>() {}
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<AfterDrain>();
    send::<KevyCommands>();
    send_sync::<RuntimeState>();
    send_sync::<verb_meta::VerbMeta>();
};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_op_table;
#[cfg(test)]
mod tests_verb_meta;

/// Queue a SEGMENTED frame for the reactor to log after this tick.
pub(crate) fn kevy_rt_push_tick_frame(seg_file: &str) {
    let argv = kevy_persist::segmented_argv(seg_file.as_bytes());
    kevy_rt::propagation::push_tick_frame(argv.iter().map(|a| a.to_vec()).collect());
}

/// `[persistence] replay_resync` as the runtime's replay mode.
fn replay_mode(cfg: &kevy_config::Config) -> kevy_persist::ReplayMode {
    if cfg.persistence.replay_resync {
        kevy_persist::ReplayMode::Resync
    } else {
        kevy_persist::ReplayMode::Strict
    }
}
