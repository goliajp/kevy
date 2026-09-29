//! kevy-elect — quorum-based primary failover for kevy.
//!
//! A layer on top of the manual
//! `REPLICAOF` primitive. Detects a primary's death by quorum
//! heartbeat, runs an offset-ordered election among the live
//! replicas, promotes the winner via `REPLICAOF NO ONE`, and
//! retargets the survivors at the new primary. Driven by an
//! operator-declared peer list (no gossip discovery — the peer set
//! is static for the lifetime of a cluster generation).
//!
//! The protocol spec lives in `docs/protocol.md`; message
//! types in [`mod@message`]. The heartbeat loop, DOWN detector, and
//! election machinery build on top of those.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
pub mod elector;
mod elector_inbound;
mod link;
pub mod message;
pub mod persist;
#[cfg(test)]
pub mod sim;
pub mod transport;
mod transport_loops;
pub mod wire;

pub use link::SecureLinks;
pub use transport::{ElectorSnapshot, PeerAddr, TopologyCallback, Transport};

#[cfg(test)]
#[path = "elector_tests.rs"]
mod elector_tests;

pub use elector::{ElectConfig, ElectJitter, Elector, Outbound};
pub use message::{Message, Role};
pub use persist::{ElectorPersist, NoPersist};
pub use wire::DecodeError;

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<ElectConfig>();
    send_sync::<ElectJitter>();
    send_sync::<Outbound>();
    send_sync::<Message>();
    send_sync::<Role>();
    send_sync::<DecodeError>();
    send_sync::<SecureLinks>();
    send_sync::<PeerAddr>();
    send_sync::<ElectorSnapshot>();
    send_sync::<Transport>();
    send_sync::<NoPersist>();
    // the persistence backend is `dyn ElectorPersist + Send`, so the
    // elector moves between threads but is shared only behind a lock
    send::<Elector>();
};
