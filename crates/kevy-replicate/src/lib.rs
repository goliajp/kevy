//! kevy-replicate — primary-to-replica streaming replication.
//!
//! Phase 1 of the v3-cluster series: one primary streams every applied
//! mutation to N read replicas over a long-lived TCP connection, using a
//! RESP3-extended frame format with an offset envelope. New replicas join
//! via an inline snapshot ship, then catch up from the live frame stream.
//!
//! - [`wire`] — RESP-based frame format (see `docs/wire.md`).
//! - [`source`] — primary-side bounded backlog indexed by offset.
//! - [`handshake`] — `REPLICATE FROM <offset> ID <id>` parse + `+ACK` format.
//! - [`slot`] — per-replica state + reconnect-window expiry.
//! - [`replica`] — replica-side blocking TCP client (handshake +
//!   frame-decoding iterator). Snapshot-ship modules land in
//!   subsequent tasks of plan
//!   `.claude/plans/2026-06-18-v3-cluster-plan.md`.
//!
//! # Applying replicated frames (T1.19)
//!
//! `ReplicaClient` yields decoded `(offset, Argv)` tuples; *applying*
//! them to a local store is the caller's responsibility — the right
//! dispatcher depends on where the replica's data lives. The wire
//! format intentionally carries the exact RESP argv the primary
//! applied, so any dispatcher that hands `Argv` through Redis-verb
//! routing produces a byte-equivalent local store.
//!
//! The canonical in-process recipe — drop into a fresh
//! `kevy::KeyspaceStore` and call `kevy::dispatch`:
//!
//! ```ignore
//! use kevy_replicate::replica::ReplicaClient;
//! let mut client = ReplicaClient::connect(("primary:16004"), "replica-a", 0)?;
//! let mut store = kevy::KeyspaceStore::new();
//! for result in &mut client {
//!     let frame = result?;
//!     kevy::dispatch(&mut store, &frame.argv);
//! }
//! # Ok::<_, kevy_replicate::replica::ReplicaError>(())
//! ```
//!
//! See the `replica_apply_dispatch_mirrors_primary_store` integration
//! test in `crates/kevy/tests/replication.rs` for the pattern under
//! the full primary+replica end-to-end harness.
//!
//! Full **server-as-replica** mode (the kevy binary auto-spawns a
//! per-shard `ReplicaClient` when `[replication] role = "replica"`,
//! routing frames into the reactor via the cross-shard ring with
//! re-replication suppression) is Phase 1.F work (T1.28-30). v1.18.0
//! supports the in-process recipe above for any user that wants to
//! drive replication themselves.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod handshake;
pub mod replica;
mod replica_decode;
pub mod slot;
pub mod source;
pub mod wire;
mod wire_snapshot;
