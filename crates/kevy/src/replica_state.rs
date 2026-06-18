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
    let mut guard = REPLICA_RUNNERS.lock().expect("REPLICA_RUNNERS poisoned");
    let runners = std::mem::take(&mut *guard);
    drop(guard); // release the lock before potentially-blocking joins
    for r in runners {
        r.shutdown();
    }
    *REPLICA_UPSTREAM.lock().expect("REPLICA_UPSTREAM poisoned") = None;
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
    let mut new_runners = Vec::with_capacity(senders.len());
    let (host, port_base) = upstream;
    for (shard_id, sender) in senders.into_iter().enumerate() {
        let port = port_base.saturating_add(u16::try_from(shard_id).unwrap_or(u16::MAX));
        let replica_id = format!("kevy-replica-{shard_id}");
        new_runners.push(ReplicaRunner::spawn((host, port), replica_id, sender));
    }
    *REPLICA_RUNNERS.lock().expect("REPLICA_RUNNERS poisoned") = new_runners;
    *REPLICA_UPSTREAM.lock().expect("REPLICA_UPSTREAM poisoned") = Some(upstream);
    Ok(())
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
    fn start_runners_without_senders_errors() {
        let _g = TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        install_senders(Vec::new());
        let result = start_runners((IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 6400));
        assert!(result.is_err());
    }
}
