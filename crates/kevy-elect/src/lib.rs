//! kevy-elect — quorum-based primary failover for kevy.
//!
//! The v3-cluster Phase 1.5 layer on top of the v1.18 manual
//! `REPLICAOF` primitive. Detects a primary's death by quorum
//! heartbeat, runs an offset-ordered election among the live
//! replicas, promotes the winner via `REPLICAOF NO ONE`, and
//! retargets the survivors at the new primary. Driven by an
//! operator-declared peer list (no gossip discovery — the peer set
//! is static for the lifetime of a cluster generation).
//!
//! T1.5.3 protocol spec lives in `docs/protocol.md`; T1.5.4 message
//! types in [`mod@message`]. T1.5.6+ (heartbeat loop, DOWN detector,
//! election machinery) land on top of those.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod elector;
mod elector_inbound;
pub mod message;
#[cfg(test)]
pub mod sim;
pub mod transport;
pub mod wire;

pub use transport::{ElectorSnapshot, PeerAddr, Transport};

#[cfg(test)]
#[path = "elector_tests.rs"]
mod elector_tests;

pub use elector::{ElectConfig, ElectJitter, Elector, Outbound};
pub use message::{Message, Role};
pub use wire::{DecodeError, decode, encode};
