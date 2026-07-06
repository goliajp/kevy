//! Process-global replica runner state (T1.29.5 / T1.30) — the slot
//! `REPLICAOF` reaches into to start / stop / replace runner threads
//! at runtime, without changing already-running `Shard`s.
//!
//! Architecture: `kevy::serve` always allocates `nshards` replica
//! inbox pairs at startup (regardless of `[replication] role`); the
//! receivers flow into the runtime via `Runtime::with_replica_inboxes`
//! and the senders are stashed here in [`REPLICA_SENDERS`]. Each
//! shard's `Shard.replica_inbox` is therefore always installed and
//! always cheap to drain (one `Option::is_some` check costs nothing
//! when empty).
//!
//! With senders process-global, [`start_runners`] is callable from
//! anywhere (the initial `[replication]` bring-up in
//! [`crate::replication`] uses it; `cmd_replicaof` calls it; the
//! `Drop` path on shutdown calls [`stop_runners`]). The shard side
//! sees no change between startup and runtime retarget — it's just
//! "drain the inbox" every tick.

use std::net::IpAddr;
use std::sync::Mutex;

use kevy_rt::ReplicaInboxSender;

use crate::replica_runner::ReplicaRunner;

/// One per-shard sender to the matching shard's [`kevy_rt::ReplicaInboxReceiver`].
/// Length = `nshards`; index = shard id. Populated once by
/// [`install_senders`] from `kevy::serve` at startup, never resized.
static REPLICA_SENDERS: Mutex<Vec<ReplicaInboxSender>> = Mutex::new(Vec::new());

/// Live runner threads. `nshards` entries when a replica role is
/// active (each connecting to one upstream shard port); empty
/// otherwise. Mutated by [`start_runners`] and [`stop_runners`] —
/// `REPLICAOF` retarget is "stop_runners + start_runners".
static REPLICA_RUNNERS: Mutex<Vec<ReplicaRunner>> = Mutex::new(Vec::new());

/// Current upstream `(host, port_base)` — `None` when not running as
/// a replica. Exposed via [`current_upstream`] so `ROLE` and (later)
/// `INFO replication` can report it.
static REPLICA_UPSTREAM: Mutex<Option<(IpAddr, u16)>> = Mutex::new(None);

/// Install the per-shard senders. Called once by `kevy::serve` after
/// building the inbox pairs; the receivers go to the runtime via
/// `Runtime::with_replica_inboxes` and the senders stay here for
/// runners to reach.
pub(crate) fn install_senders(senders: Vec<ReplicaInboxSender>) {
    // A new serve session starts as a primary until its own config /
    // REPLICAOF says otherwise — same overwrite-on-serve convention
    // as the senders themselves (keeps sequential in-process servers,
    // e.g. the integration suite, from inheriting a stale role).
    IS_REPLICA.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut guard = REPLICA_SENDERS.lock().expect("REPLICA_SENDERS poisoned");
    *guard = senders;
}

/// Return a fresh `Vec` of per-shard sender clones (one per shard,
/// in shard order). Used by [`start_runners`]. Returns an empty Vec
/// when senders haven't been installed yet (e.g. embedded use of
/// `dispatch` without `serve`).
pub(crate) fn senders_clone() -> Vec<ReplicaInboxSender> {
    REPLICA_SENDERS
        .lock()
        .expect("REPLICA_SENDERS poisoned")
        .clone()
}

/// Stop every active runner thread, joining each. After this returns,
/// `is_replica_active()` is `false` and the upstream slot is `None`.
/// Called by `REPLICAOF NO ONE`, by retarget (before `start_runners`
/// puts the new ones), and on process shutdown.
pub(crate) fn stop_runners() {
    IS_REPLICA.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut guard = REPLICA_RUNNERS.lock().expect("REPLICA_RUNNERS poisoned");
    let runners = std::mem::take(&mut *guard);
    drop(guard); // release the lock before potentially-blocking joins
    for r in runners {
        r.shutdown();
    }
    *REPLICA_UPSTREAM.lock().expect("REPLICA_UPSTREAM poisoned") = None;
    APPLIED_RUNNER_OFFSETS
        .lock()
        .expect("APPLIED_RUNNER_OFFSETS poisoned")
        .clear();
}

/// Replace the active runner set with a fresh fleet pointing at
/// `(upstream_host, upstream_port_base)`. Each shard `i` gets a
/// runner connecting to `(upstream_host, upstream_port_base + i)`.
/// Idempotent w.r.t. previous fleet — any existing runners are
/// shut down before the new fleet spawns.
///
/// Returns `Err` only when the per-shard senders haven't been
/// installed (`kevy::serve` wasn't used / pre-startup) — every other
/// failure mode is the runner thread's reconnect loop handling
/// transient upstream unreachability.
pub(crate) fn start_runners(upstream: (IpAddr, u16)) -> Result<(), &'static str> {
    let senders = senders_clone();
    if senders.is_empty() {
        return Err("replica senders not installed (kevy::serve required)");
    }
    // Stop any prior fleet before installing the new one. The old
    // runners' threads block on `next_event` reads; shutdown()
    // shuts down their sockets so the reads unblock and join
    // completes within ~one event.
    stop_runners();
    let (host, port_base) = upstream;
    // Size the per-runner applied-offset registry BEFORE spawning —
    // a runner's first heartbeat may land before this function
    // returns, and its slot must already exist.
    let runner_count = if single_source() { 1 } else { senders.len() };
    *APPLIED_RUNNER_OFFSETS
        .lock()
        .expect("APPLIED_RUNNER_OFFSETS poisoned") = vec![0; runner_count];
    let new_runners = if single_source() {
        // v3.2 embedded-as-primary: ONE upstream port, one stream —
        // a single routing runner fans into every shard inbox.
        vec![ReplicaRunner::spawn_routed(
            (host, port_base),
            "kevy-replica-single".to_string(),
            senders,
            0,
        )]
    } else {
        let mut fleet = Vec::with_capacity(senders.len());
        for (shard_id, sender) in senders.into_iter().enumerate() {
            let port = port_base.saturating_add(u16::try_from(shard_id).unwrap_or(u16::MAX));
            let replica_id = format!("kevy-replica-{shard_id}");
            fleet.push(ReplicaRunner::spawn((host, port), replica_id, sender, shard_id));
        }
        fleet
    };
    *REPLICA_RUNNERS.lock().expect("REPLICA_RUNNERS poisoned") = new_runners;
    *REPLICA_UPSTREAM.lock().expect("REPLICA_UPSTREAM poisoned") = Some(upstream);
    // AFTER the internal stop_runners above (which clears the flag) —
    // the role flips to replica only once the new fleet is installed.
    IS_REPLICA.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// v3.2 — process-wide single-source flag (config
/// `--replica-single-source`; set once by `kevy::serve`).
static SINGLE_SOURCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// v3.14 A0 — hot-path role flag: `true` while replica runners are
/// active. Read every client write via `Commands::write_denied`, so
/// it's an atomic, not the REPLICA_UPSTREAM mutex.
static IS_REPLICA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `replica-read-only` config (default ON, Redis-compatible).
static READ_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(crate) fn is_replica() -> bool {
    IS_REPLICA.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_read_only(on: bool) {
    READ_ONLY.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn read_only() -> bool {
    READ_ONLY.load(std::sync::atomic::Ordering::Relaxed)
}

/// v3.14 D3 — replica-side heartbeat view, written by every runner on
/// each `+PING`, read by INFO replication. Atomics (not a mutex): the
/// runners tick at 1Hz × shards and INFO reads are rare, but the
/// fields are independent gauges so torn reads across fields are
/// harmless.
static PRIMARY_OFFSET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static APPLIED_OFFSET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_PING_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// v3.15 D1 — per-runner applied-offset registry, `runner_slot` →
/// this runner's replication-stream position. Sized by
/// [`start_runners`] (one slot per runner: `nshards` in the fleet
/// model, 1 in single-source), cleared by [`stop_runners`].
///
/// Why a second registry next to [`APPLIED_OFFSET`]: that gauge is a
/// `fetch_max` ACROSS runners — fine for INFO's "representative lag"
/// but wrong for election candidate ordering, where the metric must
/// be the SUM of stream positions (same shape as the primary side's
/// per-shard `master_repl_offset` sum in `elect_integration`). Max
/// would let a node with ONE advanced shard tie a node where every
/// shard is caught up.
static APPLIED_RUNNER_OFFSETS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Runner-side: record one heartbeat observation. `primary_offset` is
/// summed across runners in spirit — with the per-shard fleet each
/// runner sees its own shard's stream, so we track the MAX per field
/// (INFO reports link health + a representative lag, not a per-shard
/// table; the primary side owns the authoritative per-replica table).
///
/// `runner_slot` additionally files `applied` into this runner's own
/// [`APPLIED_RUNNER_OFFSETS`] slot — the exact (non-max) per-stream
/// position the election-offset sum is built from. Plain store, not
/// max: a resync-from-0 after a diverged rejoin genuinely rewinds
/// the stream position and the election metric must tell the truth.
pub(crate) fn record_ping(runner_slot: usize, primary_offset: u64, applied: u64) {
    PRIMARY_OFFSET.fetch_max(primary_offset, std::sync::atomic::Ordering::Relaxed);
    APPLIED_OFFSET.fetch_max(applied, std::sync::atomic::Ordering::Relaxed);
    LAST_PING_MS.store(epoch_ms(), std::sync::atomic::Ordering::Relaxed);
    record_applied(runner_slot, applied);
}

/// Runner-side: refresh this runner's applied slot without a full
/// heartbeat observation — called on the 100 ms periodic ACK in the
/// frame-apply path, so the election offset stays fresh under write
/// load (pings alone only tick at 1 Hz).
pub(crate) fn record_applied(runner_slot: usize, applied: u64) {
    let mut slots = APPLIED_RUNNER_OFFSETS
        .lock()
        .expect("APPLIED_RUNNER_OFFSETS poisoned");
    if let Some(slot) = slots.get_mut(runner_slot) {
        *slot = applied;
    }
}

/// Sum of every runner's applied stream position — the replica-side
/// election offset (v3.15 D1). Comparable with the primary side's
/// per-shard `master_repl_offset` sum: both count "replication-stream
/// position, totalled across streams", and on a fully-caught-up
/// replica the two sums are equal. 0 when no runners are active.
pub(crate) fn applied_offset_sum() -> u64 {
    APPLIED_RUNNER_OFFSETS
        .lock()
        .expect("APPLIED_RUNNER_OFFSETS poisoned")
        .iter()
        .fold(0u64, |acc, v| acc.saturating_add(*v))
}

/// INFO replication: `(link_up, applied_offset, lag_frames, last_io_secs)`.
/// Link is up when a heartbeat landed within the last 3s.
pub(crate) fn replica_link_view() -> (bool, u64, u64, u64) {
    let last = LAST_PING_MS.load(std::sync::atomic::Ordering::Relaxed);
    let now = epoch_ms();
    let age_ms = now.saturating_sub(last);
    let up = last != 0 && age_ms < 3_000;
    let primary = PRIMARY_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
    let applied = APPLIED_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
    (up, applied, primary.saturating_sub(applied), age_ms / 1000)
}

/// v3.14 D5 — `min-replicas-to-write` (0 = off).
static MIN_REPLICAS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn set_min_replicas(n: u32) {
    MIN_REPLICAS.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// v3.15 D3 — planned-failover quiesce: while `Some(target)`, every
/// client write answers `-QUIESCED migrating to <target>` (the
/// cluster-rw client retries with backoff and follows). Cleared on
/// completion or abort.
static QUIESCE_TO: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn set_quiesce(target: Option<String>) {
    *QUIESCE_TO.lock().expect("QUIESCE_TO poisoned") = target;
    QUIESCED.store(
        QUIESCE_TO.lock().expect("QUIESCE_TO poisoned").is_some(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Hot-path flag mirroring `QUIESCE_TO.is_some()` (the mutex is only
/// taken to render the error text on the cold rejected path).
static QUIESCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn quiesce_active() -> bool {
    QUIESCED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn write_denied_reply() -> Option<Vec<u8>> {
    if QUIESCED.load(std::sync::atomic::Ordering::Relaxed) {
        let g = QUIESCE_TO.lock().expect("QUIESCE_TO poisoned");
        if let Some(t) = g.as_ref() {
            return Some(format!("-QUIESCED migrating to {t}\r\n").into_bytes());
        }
    }
    if IS_REPLICA.load(std::sync::atomic::Ordering::Relaxed) {
        if READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
            return Some(b"-READONLY You can't write against a read only replica.\r\n".to_vec());
        }
        return None;
    }
    let min = MIN_REPLICAS.load(std::sync::atomic::Ordering::Relaxed);
    if min > 0 && crate::ops::replication::healthy_replica_count() < min as usize {
        return Some(b"-NOREPLICAS Not enough good replicas to write.\r\n".to_vec());
    }
    None
}

/// Set at serve() from config.
pub(crate) fn set_single_source(on: bool) {
    SINGLE_SOURCE.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn single_source() -> bool {
    SINGLE_SOURCE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read the current upstream — `(host, port_base)` when running as a
/// replica, `None` otherwise. Used by `ROLE` / `INFO replication` to
/// report the live (not startup-config) upstream.
pub(crate) fn current_upstream() -> Option<(IpAddr, u16)> {
    *REPLICA_UPSTREAM
        .lock()
        .expect("REPLICA_UPSTREAM poisoned")
}

/// Test-only mutex serialising every unit test that touches the
/// process-global state (`REPLICA_SENDERS`, `REPLICA_RUNNERS`,
/// `REPLICA_UPSTREAM`). Exposed via [`crate::replica_state`] so
/// sibling test modules (e.g. `ops::replication`) share the same
/// lock — without that, ROLE tests racing REPLICAOF tests pick up
/// each other's live state.
#[cfg(test)]
pub(crate) static TEST_STATE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_empty() {
        let _g = TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        stop_runners();
        install_senders(Vec::new());
        assert!(senders_clone().is_empty());
        assert!(current_upstream().is_none());
    }

    #[test]
    fn applied_offset_sum_is_per_runner_sum_not_max() {
        let _g = TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        stop_runners();
        assert_eq!(applied_offset_sum(), 0, "no runners → 0");
        // Simulate a 3-runner fleet's registry (start_runners sizes
        // this in production).
        *APPLIED_RUNNER_OFFSETS.lock().unwrap() = vec![0; 3];
        record_ping(0, 100, 40);
        record_applied(1, 25);
        record_applied(2, 35);
        assert_eq!(applied_offset_sum(), 100, "sum across runners, not max");
        // Plain store semantics: a resync rewind must show through.
        record_applied(1, 5);
        assert_eq!(applied_offset_sum(), 80);
        // Out-of-range slot is ignored (registry resize race guard).
        record_applied(9, 1_000);
        assert_eq!(applied_offset_sum(), 80);
        // stop_runners clears the registry.
        stop_runners();
        assert_eq!(applied_offset_sum(), 0);
    }

    #[test]
    fn start_runners_without_senders_errors() {
        let _g = TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        install_senders(Vec::new());
        let result = start_runners((IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 6400));
        assert!(result.is_err());
    }
}
