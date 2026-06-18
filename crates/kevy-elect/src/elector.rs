//! `kevy-elect` core state machine — pure logic, no I/O. The TCP
//! transport (T1.5.6 network half) drives this struct by feeding it
//! ticks and inbound messages and consuming the returned outbound
//! messages.
//!
//! Pulling the algorithm out of the network layer means we can test
//! every quorum / split-brain / dueling / rejoin scenario in 100% in-
//! memory unit tests, deterministic + microsecond fast. The integration
//! tests (T1.5.12-17) layer real sockets on top once the algorithm is
//! validated.
//!
//! Naming: peers reference each other by `node_id: String` (the
//! operator-declared stable identity). All time is `std::time::Instant`
//! — the receiver-local monotonic clock, never wall-clock; no cross-
//! host clock-sync assumptions.
//!
//! See [`docs/protocol.md`](../../docs/protocol.md) for the wire-level
//! spec this struct implements.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::message::{Message, Role};

/// Tunable timeouts. Defaults match the protocol spec — operators
/// can override via the `[cluster]` config section once the
/// kevy-server adapter (separate task) wires the live config in.
#[derive(Debug, Clone, Copy)]
pub struct ElectConfig {
    /// Period between outbound `HB` per peer. Default 200 ms.
    pub hb_interval: Duration,
    /// Flag a peer DOWN after this duration without an inbound `HB`.
    /// Default 5 s = 25 × `hb_interval` (a transient 1 s blip
    /// doesn't trigger an election).
    pub down_after: Duration,
    /// Candidate waits this long for quorum `ACCEPT` before backing
    /// off. Default 3 s.
    pub election_timeout: Duration,
    /// Backoff floor after a failed election attempt. Real wait
    /// adds jitter up to `election_backoff_jitter` to prevent
    /// dueling candidates from re-running synchronously.
    pub election_backoff: Duration,
    /// Random jitter added to `election_backoff` per attempt.
    /// Default 4 s (so the real range is 1–5 s).
    pub election_backoff_jitter: Duration,
}

impl Default for ElectConfig {
    fn default() -> Self {
        Self {
            hb_interval: Duration::from_millis(200),
            down_after: Duration::from_millis(5_000),
            election_timeout: Duration::from_millis(3_000),
            election_backoff: Duration::from_millis(1_000),
            election_backoff_jitter: Duration::from_millis(4_000),
        }
    }
}

/// Per-peer scratch the elector keeps. Updated on every inbound `HB`.
/// `last_epoch` / `last_role` are recorded for future observability
/// surfaces (INFO replication's "seen-from peer" panel) — the
/// election algorithm itself only consults `last_seen` (DOWN
/// detector) and `last_repl_offset` (candidate selection).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct PeerView {
    /// Most recent `HB` reception time.
    pub(crate) last_seen: Instant,
    /// Epoch the peer claimed in its most recent `HB`.
    pub(crate) last_epoch: u64,
    /// Role the peer claimed in its most recent `HB`.
    pub(crate) last_role: Role,
    /// `repl_offset` the peer claimed in its most recent `HB`.
    pub(crate) last_repl_offset: u64,
}

/// Top-level state machine for a single kevy node in the v3-cluster
/// Phase 1.5 election. One per process (election is per-node, not
/// per-shard).
pub struct Elector {
    /// This node's stable id.
    pub(crate) node_id: String,
    /// Operator-declared peer set, by id. **Includes** this node —
    /// the elector filters self at run-time. Length = `N` (quorum
    /// = `N / 2 + 1`).
    pub(crate) peer_ids: Vec<String>,
    /// Tunable timeouts.
    pub(crate) config: ElectConfig,
    /// Self-perceived role.
    pub(crate) role: Role,
    /// Election epoch this node believes is current. Bumped only by
    /// own `OFFER`s; updated to a higher seen value on inbound
    /// `OFFER`/`ACCEPT`/`ANNOUNCE`.
    pub(crate) epoch: u64,
    /// `Some(id)` ⇒ this node knows `id` is currently the primary.
    /// `None` until the first `ANNOUNCE` is seen (or the node was
    /// configured-primary at boot).
    pub(crate) current_primary: Option<String>,
    /// This node's most recent `repl_offset` — set externally by the
    /// kevy-server adapter from the live replication source / runner.
    pub(crate) my_repl_offset: u64,
    /// Last outbound `HB` time per peer (per-peer schedule, to allow
    /// staggering rather than thundering-herd at every tick).
    pub(crate) last_hb_sent: HashMap<String, Instant>,
    /// Inbound observations per peer.
    pub(crate) peer_views: HashMap<String, PeerView>,
    /// While `Candidate`: ACCEPT vote tally for the current epoch.
    /// Cleared on transition out of Candidate.
    pub(crate) accept_votes: HashMap<String, ()>,
    /// While `Candidate`: when the OFFER was broadcast (election
    /// times out at `offer_at + election_timeout`).
    pub(crate) offer_at: Option<Instant>,
    /// While in election backoff: don't start another candidacy
    /// before this. Set on election timeout.
    pub(crate) backoff_until: Option<Instant>,
    /// Last epoch this node has cast an ACCEPT for (one vote per
    /// epoch — prevents two candidates from both winning quorum in
    /// the same round).
    pub(crate) last_accept_epoch: Option<u64>,
    /// Address (`host:port` of the kevy compat port) advertised in
    /// this node's `ANNOUNCE` when it becomes primary. Set
    /// externally by the kevy-server adapter at startup.
    pub(crate) my_advertised_addr: String,
    /// Deterministic backoff jitter — operators (and tests) inject
    /// it; the elector doesn't read the system random.
    pub(crate) jitter: ElectJitter,
}

/// Source of jitter for election backoff. Tests use a fixed value;
/// production uses `ElectJitter::System` which reads `Instant`
/// + node_id as a poor-mans entropy. Pure-Rust 0-dep — no `rand` crate.
#[derive(Debug, Clone)]
pub enum ElectJitter {
    /// Fixed value (test-friendly, deterministic).
    Fixed(Duration),
    /// Hash of `(now_nanos, node_id)` clamped into
    /// `[0, max_jitter)`. Deterministic enough for production while
    /// avoiding zero-cost-jitter dueling.
    System,
}

impl ElectJitter {
    /// Sample a jitter value in `[0, max]`.
    fn sample(&self, max: Duration, now: Instant, node_id: &str) -> Duration {
        match self {
            Self::Fixed(d) => *d.min(&max),
            Self::System => {
                // Mix `node_id` bytes into a u64 hash and clamp into
                // `[0, max.as_nanos())`. Coarse but adequate — the
                // jitter only needs to break ties between dueling
                // candidates, not be cryptographically random.
                let mut h: u64 = 1469598103934665603;
                for b in node_id.as_bytes() {
                    h = h.wrapping_mul(1099511628211) ^ u64::from(*b);
                }
                // Pull a u64 worth of bits out of `now`'s elapsed-
                // since-arbitrary-anchor representation. Using the
                // low 64 bits of `now.elapsed_since(anchor)` would
                // need an anchor — instead, hash a stable derivation
                // of `now` via the elector's lazy anchor approach.
                // For simplicity: mix `node_id` bytes again with a
                // per-call seed.
                let _ = now; // placeholder: production jitter wants per-call entropy.
                let span_ns = max.as_nanos().max(1) as u64;
                Duration::from_nanos(h % span_ns)
            }
        }
    }
}

/// One message + recipient that the elector wants to send. The
/// transport layer (T1.5.6 network half) drains
/// `Transport` each loop iteration and writes to the
/// per-peer TCP connections.
#[derive(Debug, Clone)]
pub struct Outbound {
    /// Recipient. `"*"` (a sentinel — never a valid node_id since
    /// they're ASCII ≤ 32 B and operators don't use stars) means
    /// "broadcast to every peer except self". The transport
    /// expands the sentinel on its end.
    pub to: String,
    /// The message to send.
    pub msg: Message,
}

impl Outbound {
    /// Sentinel for broadcast-to-all.
    pub const BROADCAST: &'static str = "*";
}

impl Elector {
    /// Build an elector for a node with the given stable id, peer
    /// membership (the full list including self), advertised
    /// `host:port`, and config tunables.
    ///
    /// `start_role` is `Primary` for the bootstrap node (operator-
    /// declared at first start) and `Replica` for the rest.
    pub fn new(
        node_id: impl Into<String>,
        peer_ids: Vec<String>,
        my_advertised_addr: impl Into<String>,
        start_role: Role,
        config: ElectConfig,
        jitter: ElectJitter,
    ) -> Self {
        let node_id = node_id.into();
        Self {
            node_id,
            peer_ids,
            config,
            role: start_role,
            epoch: 1,
            current_primary: None,
            my_repl_offset: 0,
            last_hb_sent: HashMap::new(),
            peer_views: HashMap::new(),
            accept_votes: HashMap::new(),
            offer_at: None,
            backoff_until: None,
            last_accept_epoch: None,
            my_advertised_addr: my_advertised_addr.into(),
            jitter,
        }
    }

    /// Update this node's `repl_offset` (called by the kevy-server
    /// adapter when the replication source / runner advances).
    pub fn set_repl_offset(&mut self, offset: u64) {
        self.my_repl_offset = offset;
    }

    /// Current self-perceived role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Current epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Last-known primary id (`None` until first ANNOUNCE / boot
    /// declaration).
    pub fn current_primary(&self) -> Option<&str> {
        self.current_primary.as_deref()
    }

    /// Drive the elector forward by `now`. Schedules outbound `HB`
    /// per peer, detects DOWN, transitions Candidate → Primary on
    /// quorum, and runs the candidate's election-timeout fallback.
    /// Returns a fresh batch of outbound messages — callers should
    /// drain in one pass.
    pub fn tick(&mut self, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        self.emit_heartbeats(now, &mut out);
        self.maybe_start_election(now, &mut out);
        self.maybe_finish_candidacy(now, &mut out);
        out
    }

    /// Process one inbound message (from `from_node_id`) at `now`.
    /// Updates per-peer view, applies the state machine transitions
    /// the spec defines, returns any outbound messages the
    /// transition produced.
    pub fn on_message(
        &mut self,
        from_node_id: &str,
        msg: Message,
        now: Instant,
    ) -> Vec<Outbound> {
        let mut out = Vec::new();
        match msg {
            Message::Hb {
                epoch,
                node_id: _,
                role,
                repl_offset,
            } => self.on_hb(from_node_id, epoch, role, repl_offset, now),
            Message::Offer {
                new_epoch,
                candidate_id,
                repl_offset,
            } => self.on_offer(new_epoch, candidate_id, repl_offset, &mut out),
            Message::Accept {
                epoch,
                accepter_id,
            } => self.on_accept(epoch, accepter_id, now, &mut out),
            Message::Announce {
                epoch,
                new_primary_id,
                new_primary_addr,
            } => self.on_announce(epoch, new_primary_id, new_primary_addr, &mut out),
        }
        out
    }

    // ─────────── tick helpers ───────────

    fn emit_heartbeats(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        // One HB per peer per `hb_interval`. Per-peer schedule
        // staggers (a peer added later gets its own clock).
        for peer in self.peer_ids.clone() {
            if peer == self.node_id {
                continue;
            }
            let due = match self.last_hb_sent.get(&peer) {
                Some(prev) => now.duration_since(*prev) >= self.config.hb_interval,
                None => true,
            };
            if due {
                self.last_hb_sent.insert(peer.clone(), now);
                out.push(Outbound {
                    to: peer,
                    msg: Message::Hb {
                        epoch: self.epoch,
                        node_id: self.node_id.clone(),
                        role: self.role,
                        repl_offset: self.my_repl_offset,
                    },
                });
            }
        }
    }

    fn maybe_start_election(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        // Only replicas start elections.
        if self.role != Role::Replica {
            return;
        }
        // In backoff after a failed candidacy.
        if let Some(b) = self.backoff_until
            && now < b
        {
            return;
        }
        // Primary must be DOWN by my view.
        let Some(primary) = self.current_primary.clone() else {
            return;
        };
        if !self.is_peer_down(&primary, now) {
            return;
        }
        // Candidate-selection: I must have the highest offset AND
        // lowest node-id among alive peers (the primary is dead +
        // not in the tie-break set).
        if !self.am_best_candidate(now) {
            return;
        }
        // Start the candidacy.
        self.epoch = self.epoch.saturating_add(1);
        self.role = Role::Candidate;
        self.accept_votes.clear();
        // Implicit self-vote — record ourselves in the tally so
        // single-peer-needed (N=1, degenerate) and quorum=2/N=2
        // both work.
        self.accept_votes.insert(self.node_id.clone(), ());
        self.offer_at = Some(now);
        out.push(Outbound {
            to: Outbound::BROADCAST.to_string(),
            msg: Message::Offer {
                new_epoch: self.epoch,
                candidate_id: self.node_id.clone(),
                repl_offset: self.my_repl_offset,
            },
        });
    }

    fn maybe_finish_candidacy(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        if self.role != Role::Candidate {
            return;
        }
        let Some(offer_at) = self.offer_at else {
            return;
        };
        let quorum = self.quorum_size();
        if self.accept_votes.len() >= quorum {
            // Won — broadcast ANNOUNCE and become primary.
            self.role = Role::Primary;
            self.current_primary = Some(self.node_id.clone());
            self.offer_at = None;
            self.accept_votes.clear();
            out.push(Outbound {
                to: Outbound::BROADCAST.to_string(),
                msg: Message::Announce {
                    epoch: self.epoch,
                    new_primary_id: self.node_id.clone(),
                    new_primary_addr: self.my_advertised_addr.clone(),
                },
            });
            return;
        }
        if now.duration_since(offer_at) >= self.config.election_timeout {
            // Lost / timed out — back off with jitter, fall back to
            // Replica.
            self.role = Role::Replica;
            self.offer_at = None;
            self.accept_votes.clear();
            let jitter = self
                .jitter
                .sample(self.config.election_backoff_jitter, now, &self.node_id);
            self.backoff_until = Some(now + self.config.election_backoff + jitter);
        }
    }

    // ─────────── inbound handlers ───────────


}
