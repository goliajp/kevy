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
use kevy_rt::Runtime;
use kevy_store::Store;
use kevy_sys::Socket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod cmd;
mod cmd_block;
mod metrics_http;
pub(crate) mod audit_log;
mod cmd_block_serve;
mod cmd_data;
mod cmd_hello;
mod cmd_lua;
mod cmd_resolve;
mod commands;
mod config_global;
mod replication;
mod dispatch;
mod dispatch_collections;
mod dispatch_collections_v127;
mod dispatch_resp3;
mod dispatch_geo;
mod dispatch_stream;
mod elect_integration;
mod ops;
mod replica_runner;
mod replica_state;
mod scope_integration;

pub use config_global::init as config_init;
pub use config_global::replace as config_replace;
pub use dispatch::dispatch;
pub use kevy_rt::Argv;
pub use kevy_store::Store as KeyspaceStore;

/// Test-only hook to install per-shard replica inbox senders into the
/// process-global slot (`replica_state::install_senders`). Production
/// code calls the equivalent path via `kevy::serve`'s startup; tests
/// that build a [`kevy_rt::Runtime`] directly need this to wire
/// `REPLICAOF` against their own receivers.
#[doc(hidden)]
pub fn install_replica_senders_for_test(senders: Vec<kevy_rt::ReplicaInboxSender>) {
    replica_state::install_senders(senders);
}

/// Test-only hook to install the scope_integration globals
/// without bringing up a full `kevy::serve`. Integration tests in
/// `tests/scope_*.rs` use this to verify routing on a single
/// Runtime. Calls into `scope_integration::install` and
/// `install_self_id`; idempotent because both are OnceLocks.
/// Returns the install_err if `[cluster] scopes` is malformed.
#[doc(hidden)]
pub fn install_scope_integration_for_test(cfg: &kevy_config::Config) -> Result<(), String> {
    scope_integration::install(cfg)?;
    scope_integration::install_self_id(cfg);
    Ok(())
}

/// What to do with a connection after draining its buffered commands.
pub enum AfterDrain {
    KeepOpen,
    Close,
}

/// kevy's command set, plugged into the `kevy-rt` runtime. Stateless — the
/// keyspace lives in each shard's `Store`, so this is a zero-sized clone target.
#[derive(Clone, Copy, Default)]
pub struct KevyCommands;


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

/// Run the thread-per-core server forever: `nshards` shards on `ip:port`,
/// snapshotting to / restoring from `data_dir`, with the AOF on/off.
///
/// Reads `cfg.persistence.appendfsync` from the process-wide config (set by
/// `config_init`) to pick the AOF fsync policy. Defaults to `EverySec`
/// when no config is installed (matches the Wave 1 behaviour).
/// **v1.39** — signal flag flipped by the SIGTERM / SIGINT handler.
/// Async-signal-safe; AtomicBool::store is signal-safe per the C
/// memory model.
#[cfg(unix)]
static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

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
    // Polling-bridge thread: signal handlers can't easily touch the
    // per-run Arc, so we poll the static AtomicBool every 100 ms and
    // mirror it into `stop`. Daemon thread; exits when the process does.
    std::thread::spawn(move || loop {
        if SIGNAL_RECEIVED.load(std::sync::atomic::Ordering::SeqCst) {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
}

#[cfg(not(unix))]
fn install_signal_handlers(_stop: Arc<AtomicBool>) {
    // No-op on non-Unix; production deployments are Unix anyway.
}

pub fn serve(ip: [u8; 4], port: u16, nshards: usize, data_dir: PathBuf, enable_aof: bool) -> ! {
    let cfg = config_global::get();
    let fsync = map_appendfsync(cfg.persistence.appendfsync);
    let mut runtime = Runtime::new(ip, port, nshards, KevyCommands)
        .with_data_dir(data_dir)
        .with_accept_shards(cfg.server.accept_shards)
        .with_max_clients(cfg.server.max_clients)
        .with_aof(enable_aof)
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
        runtime = runtime.with_cluster(cluster_port_base(&cfg));
    }
    // v1.25 UDS: opt-in via `KEVY_UNIX_SOCKET=/path/to/sock` env var. Lets
    // local clients (and benches) skip TCP loopback overhead — fair
    // comparison against valkey/redis's `unixsocket` config.
    if let Ok(path) = std::env::var("KEVY_UNIX_SOCKET") {
        if !path.is_empty() {
            runtime = runtime.with_unix_socket(PathBuf::from(path));
        }
    }
    let runtime = replication::apply(runtime, &cfg, nshards);
    // Spawn the kevy-elect control plane when the operator
    // configured `[cluster] peers = "..."` + `node_id`. Opt-in;
    // empty peers leaves the subsystem dormant.
    // Allocate per-shard offset slots first (always, even when
    // elect is dormant — cost is `nshards` AtomicU64 / process,
    // negligible).
    elect_integration::install_shard_offsets(nshards);
    elect_integration::maybe_start(&cfg);
    // Scope-routing setup. Idempotent; a bad scope config fails
    // the boot loudly here rather than at the first wrong-shard
    // write.
    if let Err(msg) = scope_integration::install(&cfg) {
        eprintln!("kevy: bad [cluster] scopes config: {msg}");
        std::process::exit(1);
    }
    scope_integration::install_self_id(&cfg);
    let stop = Arc::new(AtomicBool::new(false));
    // v1.39 — install SIGTERM + SIGINT handlers that flip `stop`,
    // triggering the runtime's existing drain path (fsync AOF, close
    // listeners, exit 0). std-only: raw `signal(2)` + a poller thread
    // that bridges the signal-safe static into the per-run `Arc`.
    install_signal_handlers(Arc::clone(&stop));
    // v1.41 — Prometheus /metrics endpoint. No-op when port = 0.
    metrics_http::spawn_if_enabled(&config_global::get());
    // v1.42 — audit log init. No-op when log_path is empty.
    audit_log::init(&config_global::get().audit.log_path);
    // Replica runners (if any) live in process-global state in
    // `replica_state` — they are started by `replication::apply` for
    // the startup `role = "replica"` path and by `REPLICAOF` at
    // runtime (T1.29.5). On exit the runners are dropped via their
    // process-global slot; the `Drop` impl signals stop + joins each
    // runner thread, so the process exits cleanly with no orphan TCP
    // fds.
    let run_result = runtime.run(stop);
    // Stop kevy-elect after the runtime exits so the control plane
    // doesn't outlive the data plane.
    elect_integration::shutdown();
    if let Err(e) = run_result {
        eprintln!("kevy: runtime error: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
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
