//! `kevy-elect` core state machine — pure logic, no I/O. The TCP
//! transport drives this struct by feeding it
//! ticks and inbound messages and consuming the returned outbound
//! messages.
//!
//! Pulling the algorithm out of the network layer means we can test
//! every quorum / split-brain / dueling / rejoin scenario in 100% in-
//! memory unit tests, deterministic + microsecond fast. The integration
//! tests layer real sockets on top once the algorithm is
//! validated.
//!
//! Naming: peers reference each other by `node_id: String` (the
//! operator-declared stable identity). All time is `std::time::Instant`
//! — the receiver-local monotonic clock, never wall-clock; no cross-
//! host clock-sync assumptions.
//!
//! See [`docs/protocol.md`](../../docs/protocol.md) for the wire-level
//! spec this struct implements.
//!
//! ```
//! use std::time::Instant;
//! use kevy_elect::elector::{Elector, Outbound};
//! use kevy_elect::{Message, Role};
//!
//! // the caller owns the clock and the network: feed ticks and messages in,
//! // send whatever comes out
//! let mut a = Elector::new("a", vec!["a".into(), "b".into()], "a:6004", Role::Primary);
//! let out: Vec<Outbound> = a.tick(Instant::now());
//! assert_eq!(out.len(), 1);
//! assert_eq!(out[0].to, "b");
//! assert!(matches!(out[0].msg, Message::Hb { epoch: 1, .. }));
//! ```

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

pub use crate::config::{ElectConfig, ElectJitter};
use crate::elector_inbound::PeerView;
use crate::message::{Message, Role};
use crate::persist::{ElectorPersist, NoPersist};

/// Top-level state machine for a single kevy node in the v3-cluster
/// Phase 1.5 election. One per process (election is per-node, not
/// per-shard).
///
/// A one-node cluster elects itself once the cold-start grace window
/// (`down_after`) passes without a primary:
///
/// ```
/// use std::time::{Duration, Instant};
/// use kevy_elect::{Elector, Role};
///
/// let mut a = Elector::new("a", vec!["a".into()], "a:6004", Role::Replica);
/// let t0 = Instant::now();
/// a.tick(t0);
/// a.tick(t0 + Duration::from_secs(5));
/// assert_eq!((a.role(), a.current_primary(), a.epoch()), (Role::Primary, Some("a"), 2));
/// ```
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
    /// First `tick` instant — anchors the cold-start (no known
    /// primary) election grace window.
    pub(crate) first_tick: Option<Instant>,
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
    pub(crate) accept_votes: HashSet<String>,
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
    /// Durable `(epoch, voted_for)` backend. Written
    /// **before** any ACCEPT leaves the node and **before** any
    /// epoch bump/follow takes effect — Raft's persistence rule.
    /// Defaults to [`NoPersist`]; attach a real backend with
    /// [`Elector::with_persist`].
    pub(crate) persist: Box<dyn ElectorPersist + Send>,
}

/// One message + recipient that the elector wants to send. The
/// transport layer drains
/// `Transport` each loop iteration and writes to the
/// per-peer TCP connections.
///
/// ```
/// use std::time::Instant;
/// use kevy_elect::{Elector, Outbound, Role};
///
/// let mut a = Elector::new("a", vec!["a".into(), "b".into(), "c".into()], "a:6004", Role::Primary);
/// for Outbound { to, msg, .. } in a.tick(Instant::now()) {
///     // a transport would write `msg.encode()` to the link for `to`
///     assert!(to == "b" || to == "c");
///     assert!(!msg.encode().is_empty());
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Outbound {
    /// Recipient. `"*"` (a sentinel — never a valid node_id since
    /// they're ASCII ≤ 32 B and operators don't use stars) means
    /// "broadcast to every peer except self". The transport
    /// expands the sentinel on its end.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use kevy_elect::{Elector, Message, Outbound, Role};
    ///
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "a:6004", Role::Replica);
    /// let t0 = Instant::now();
    /// assert_eq!(a.tick(t0)[0].to, "b"); // a heartbeat names one peer
    /// let out = a.tick(t0 + Duration::from_secs(5));
    /// let offer = out.iter().find(|o| matches!(o.msg, Message::Offer { .. }));
    /// assert_eq!(offer.map(|o| o.to.as_str()), Some(Outbound::BROADCAST)); // an offer names all
    /// ```
    pub to: String,
    /// The message to send.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "a:6004", Role::Primary);
    /// let out = a.tick(Instant::now());
    /// let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// assert_eq!(out[0].msg, hb);
    /// ```
    pub msg: Message,
}

impl Outbound {
    /// Sentinel for broadcast-to-all.
    ///
    /// ```
    /// use kevy_elect::Outbound;
    ///
    /// // how a transport expands it
    /// fn recipients<'a>(to: &'a str, me: &str, peers: &[&'a str]) -> Vec<&'a str> {
    ///     if to == Outbound::BROADCAST {
    ///         peers.iter().copied().filter(|p| *p != me).collect()
    ///     } else {
    ///         vec![to]
    ///     }
    /// }
    /// assert_eq!(recipients("*", "a", &["a", "b", "c"]), ["b", "c"]);
    /// assert_eq!(recipients("c", "a", &["a", "b", "c"]), ["c"]);
    /// ```
    pub const BROADCAST: &'static str = "*";
}

impl Elector {
    /// Build an elector for a node with the given stable id, peer
    /// membership (the full list including self) and advertised
    /// `host:port`, with the default timeouts ([`ElectConfig::default`])
    /// and [`ElectJitter::System`]; [`Self::with_config`] and
    /// [`Self::with_jitter`] change them.
    ///
    /// `start_role` is `Primary` for the bootstrap node (operator-
    /// declared at first start) and `Replica` for the rest.
    ///
    /// ```
    /// use kevy_elect::{Elector, Role};
    ///
    /// let e = Elector::new("a", vec!["a".to_string(), "b".to_string()], "10.0.0.1:6004", Role::Replica);
    /// assert_eq!((e.role(), e.epoch()), (Role::Replica, 1));
    /// ```
    pub fn new(
        node_id: impl Into<String>,
        peer_ids: Vec<String>,
        my_advertised_addr: impl Into<String>,
        start_role: Role,
    ) -> Self {
        let config = ElectConfig::default();
        let jitter = ElectJitter::System;
        let node_id = node_id.into();
        Self {
            node_id,
            peer_ids,
            config,
            role: start_role,
            epoch: 1,
            current_primary: None,
            first_tick: None,
            my_repl_offset: 0,
            last_hb_sent: HashMap::new(),
            peer_views: HashMap::new(),
            accept_votes: HashSet::new(),
            offer_at: None,
            backoff_until: None,
            last_accept_epoch: None,
            my_advertised_addr: my_advertised_addr.into(),
            jitter,
            persist: Box::new(NoPersist),
        }
    }

    /// Replace the timeouts.
    ///
    /// ```
    /// use std::time::Duration;
    /// use kevy_elect::{ElectConfig, Elector, Role};
    ///
    /// let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(50));
    /// let e = Elector::new("a", vec!["a".to_string()], "10.0.0.1:6004", Role::Primary).with_config(cfg);
    /// assert_eq!(e.role(), Role::Primary);
    /// ```
    #[must_use]
    pub fn with_config(mut self, config: ElectConfig) -> Self {
        self.config = config;
        self
    }

    /// Replace the backoff jitter source (tests fix it for determinism).
    ///
    /// ```
    /// use std::time::Duration;
    /// use kevy_elect::{ElectJitter, Elector, Role};
    ///
    /// let e = Elector::new("a", vec!["a".to_string()], "10.0.0.1:6004", Role::Primary)
    ///     .with_jitter(ElectJitter::Fixed(Duration::ZERO));
    /// assert_eq!(e.epoch(), 1);
    /// ```
    #[must_use]
    pub fn with_jitter(mut self, jitter: ElectJitter) -> Self {
        self.jitter = jitter;
        self
    }

    /// Attach a persistence backend and restore its saved state.
    /// Restores the persisted epoch (so a restarted node
    /// never re-runs an election under an already-consumed epoch)
    /// and, when a vote was cast, re-arms the one-vote-per-epoch
    /// guard for that epoch. Call right after [`Elector::new`],
    /// before the transport starts driving the elector.
    ///
    /// ```
    /// # use std::sync::{Arc, Mutex};
    /// # use kevy_elect::ElectorPersist;
    /// # #[derive(Clone, Default)]
    /// # struct Mem(Arc<Mutex<(u64, Option<String>)>>);
    /// # impl ElectorPersist for Mem {
    /// #     fn save(&self, epoch: u64, voted_for: Option<&str>) {
    /// #         *self.0.lock().unwrap() = (epoch, voted_for.map(str::to_string));
    /// #     }
    /// #     fn load(&self) -> (u64, Option<String>) {
    /// #         self.0.lock().unwrap().clone()
    /// #     }
    /// # }
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let disk = Mem::default();
    /// let peers = || vec!["b".to_string(), "c".to_string()];
    /// let mut c = Elector::new("c", peers(), "c:6004", Role::Replica).with_persist(Box::new(disk.clone()));
    /// let won = Message::Announce { epoch: 5, new_primary_id: "b".into(), new_primary_addr: "b:6004".into() };
    /// c.on_message("b", won, Instant::now());
    ///
    /// // after a restart the epoch picks up where it left off
    /// let c = Elector::new("c", peers(), "c:6004", Role::Replica).with_persist(Box::new(disk));
    /// assert_eq!(c.epoch(), 5);
    /// ```
    #[must_use]
    pub fn with_persist(mut self, persist: Box<dyn ElectorPersist + Send>) -> Self {
        let (epoch, voted_for) = persist.load();
        if epoch > 0 {
            self.epoch = self.epoch.max(epoch);
            if voted_for.is_some() {
                self.last_accept_epoch = Some(epoch);
            }
        }
        self.persist = persist;
        self
    }

    /// Update this node's `repl_offset` (called by the kevy-server
    /// adapter when the replication source / runner advances).
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "c:6004", Role::Replica);
    /// c.set_repl_offset(900);
    /// // a candidate with less data than c gets no vote from it
    /// let offer = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 800 };
    /// assert!(c.on_message("b", offer, Instant::now()).is_empty());
    /// ```
    pub fn set_repl_offset(&mut self, offset: u64) {
        self.my_repl_offset = offset;
    }

    /// Current self-perceived role.
    ///
    /// ```
    /// use kevy_elect::{Elector, Role};
    ///
    /// let e = Elector::new("a", vec!["a".into(), "b".into()], "a:6004", Role::Replica);
    /// assert_eq!(e.role(), Role::Replica);
    /// ```
    pub fn role(&self) -> Role {
        self.role
    }

    /// Current epoch.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "c:6004", Role::Replica);
    /// assert_eq!(c.epoch(), 1); // every node boots in epoch 1
    /// let offer = Message::Offer { new_epoch: 3, candidate_id: "b".into(), repl_offset: 0 };
    /// c.on_message("b", offer, Instant::now());
    /// assert_eq!(c.epoch(), 3); // voting moves it to the candidate's epoch
    /// ```
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Last-known primary id (`None` until first ANNOUNCE / boot
    /// declaration).
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica);
    /// assert_eq!(b.current_primary(), None);
    /// let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// b.on_message("a", hb, Instant::now());
    /// assert_eq!(b.current_primary(), Some("a"));
    /// ```
    pub fn current_primary(&self) -> Option<&str> {
        self.current_primary.as_deref()
    }

    /// Quorum visibility for the primary lease: does this
    /// node currently see a strict majority of the cluster (itself +
    /// peers heard from within `down_after`)? A primary that answers
    /// `false` here is on the minority side of a partition and must
    /// fence writes — the lease window is exactly `down_after`.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into(), "c".into()], "a:6004", Role::Primary);
    /// let t0 = Instant::now();
    /// assert!(!a.has_quorum(t0)); // alone: 1 of 3
    /// let hb = Message::Hb { epoch: 1, node_id: "b".into(), role: Role::Replica, repl_offset: 0 };
    /// a.on_message("b", hb, t0);
    /// assert!(a.has_quorum(t0)); // 2 of 3
    /// assert!(!a.has_quorum(t0 + Duration::from_secs(5))); // b's lease ran out
    /// ```
    pub fn has_quorum(&self, now: Instant) -> bool {
        let reachable = self
            .peer_views
            .values()
            .filter(|v| now.duration_since(v.last_seen) < self.config.down_after)
            .count();
        let cluster = self.peer_ids.len().max(1);
        (reachable + 1) * 2 > cluster
    }

    /// Drive the elector forward by `now`. Schedules outbound `HB`
    /// per peer, detects DOWN, transitions Candidate → Primary on
    /// quorum, and runs the candidate's election-timeout fallback.
    /// Returns a fresh batch of outbound messages — callers should
    /// drain in one pass.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use kevy_elect::{Elector, Role};
    ///
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "a:6004", Role::Primary);
    /// let t0 = Instant::now();
    /// assert_eq!(a.tick(t0).len(), 1); // heartbeat to b
    /// assert!(a.tick(t0).is_empty()); // not due again yet
    /// assert_eq!(a.tick(t0 + Duration::from_millis(200)).len(), 1);
    /// ```
    pub fn tick(&mut self, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        if self.first_tick.is_none() {
            self.first_tick = Some(now);
        }
        self.emit_heartbeats(now, &mut out);
        self.maybe_start_election(now, &mut out);
        self.maybe_finish_candidacy(now, &mut out);
        out
    }

    /// Process one inbound message (from `from_node_id`) at `now`.
    /// Updates per-peer view, applies the state machine transitions
    /// the spec defines, returns any outbound messages the
    /// transition produced.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "c:6004", Role::Replica);
    /// let offer = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 0 };
    /// let out = c.on_message("b", offer, Instant::now());
    /// assert_eq!(out.len(), 1);
    /// assert_eq!(out[0].to, "b");
    /// assert_eq!(out[0].msg, Message::Accept { epoch: 2, accepter_id: "c".into() });
    /// ```
    pub fn on_message(&mut self, from_node_id: &str, msg: Message, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        match msg {
            Message::Hb { epoch, node_id: _, role, repl_offset } => {
                self.on_hb(from_node_id, epoch, role, repl_offset, now)
            }
            Message::Offer { new_epoch, candidate_id, repl_offset } => {
                self.on_offer(new_epoch, candidate_id, repl_offset, &mut out)
            }
            Message::Accept { epoch, accepter_id } => {
                self.on_accept(epoch, accepter_id, now, &mut out)
            }
            Message::Announce { epoch, new_primary_id, new_primary_addr } => {
                self.on_announce(epoch, &new_primary_id, new_primary_addr, &mut out)
            }
        }
        out
    }
}

impl core::fmt::Debug for Elector {
    /// Prints the whole election state except the persistence backend.
    ///
    /// `persist` is a `Box<dyn ElectorPersist + Send>`: a trait object has
    /// no `Debug`, and the backend's identity says nothing about why an
    /// election went the way it did. Everything that does — role, epoch,
    /// votes, timers — is shown.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Elector")
            .field("node_id", &self.node_id)
            .field("peer_ids", &self.peer_ids)
            .field("config", &self.config)
            .field("role", &self.role)
            .field("epoch", &self.epoch)
            .field("current_primary", &self.current_primary)
            .field("first_tick", &self.first_tick)
            .field("my_repl_offset", &self.my_repl_offset)
            .field("last_hb_sent", &self.last_hb_sent)
            .field("peer_views", &self.peer_views)
            .field("accept_votes", &self.accept_votes)
            .field("offer_at", &self.offer_at)
            .field("backoff_until", &self.backoff_until)
            .field("last_accept_epoch", &self.last_accept_epoch)
            .field("my_advertised_addr", &self.my_advertised_addr)
            .field("jitter", &self.jitter)
            .finish_non_exhaustive()
    }
}
