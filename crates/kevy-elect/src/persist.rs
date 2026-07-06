//! Durable `(epoch, voted_for)` storage — Raft's persistence rule
//! applied to the kevy election (v3.15 D1).
//!
//! Why this exists: a node that votes ACCEPT in epoch `e`, crashes,
//! and restarts with a zeroed memory could vote *again* in epoch `e`
//! for a different candidate — two candidates each collect "quorum"
//! and the cluster splits brain. Raft's fix is mechanical: `(epoch,
//! votedFor)` must hit stable storage **before** the vote reply (or
//! any frame carrying a bumped epoch) leaves the node.
//!
//! [`Elector`](crate::Elector) enforces the write discipline; this
//! module only defines the storage contract. The kevy server wires a
//! file-backed implementation (`<data_dir>/elect.meta`); tests and
//! diskless embedders use [`NoPersist`].

/// Storage contract for the elector's `(epoch, voted_for)` pair.
///
/// **Synchronous semantics**: when [`ElectorPersist::save`] returns,
/// the pair is durable (implementations fsync before returning). The
/// elector calls `save` *before* emitting any ACCEPT and *before*
/// adopting or bumping an epoch — implementations must not defer the
/// write.
pub trait ElectorPersist {
    /// Persist the pair. Returning means "durable". `voted_for` is
    /// `Some(candidate_id)` when this node has cast (or is about to
    /// cast) its vote for `epoch`, `None` when it merely follows a
    /// higher epoch without voting.
    fn save(&self, epoch: u64, voted_for: Option<&str>);

    /// Load the most recently saved pair. `(0, None)` when nothing
    /// has ever been saved (fresh node) — the elector treats epoch 0
    /// as "no persisted state" and keeps its boot default.
    fn load(&self) -> (u64, Option<String>);
}

/// The default no-op backend: nothing survives a restart. Correct
/// for unit tests and single-node / diskless embedded deployments
/// where a restarted node re-joining an election it voted in is not
/// a reachable scenario.
pub struct NoPersist;

impl ElectorPersist for NoPersist {
    fn save(&self, _epoch: u64, _voted_for: Option<&str>) {}

    fn load(&self) -> (u64, Option<String>) {
        (0, None)
    }
}
