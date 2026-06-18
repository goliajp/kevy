//! In-process N-node simulator for [`crate::Elector`]. Drives every
//! node's tick + routes their outbound messages between peers via
//! in-memory queues, with **partition** and **node-kill** chaos
//! primitives. Lets T1.5.12-17 exhaustively test the v3-cluster
//! Phase 1.5 election algorithm before the real TCP transport
//! lands — every quorum / split-brain / dueling / rejoin scenario
//! becomes a deterministic unit test.
//!
//! No threads, no sockets, no wall clocks: tests advance a virtual
//! `Instant` and call [`Sim::tick_all`] to fan messages between
//! nodes one round at a time. Round-trip = one tick of every alive
//! node, then route every produced message to its target.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::elector::{ElectConfig, ElectJitter, Elector, Outbound};
use crate::message::Role;

/// One node's place in the simulator.
pub struct SimNode {
    /// The elector this node runs.
    pub elector: Elector,
    /// `true` ⇒ this node is alive; `false` ⇒ kill_node was called
    /// and the sim skips its tick + drops any message routed to it.
    pub alive: bool,
}

/// Multi-node election simulator.
pub struct Sim {
    nodes: HashMap<String, SimNode>,
    /// Symmetric undirected partition set — `(a, b)` in the set
    /// means messages between a and b are dropped (in BOTH
    /// directions; tests add one entry per pair).
    partitions: Vec<(String, String)>,
}

impl Sim {
    /// Build an N-node sim. `nodes` lists `(node_id, start_role)`
    /// pairs; the operator's "primary at boot" choice is the only
    /// `Primary` in the list, the rest are `Replica`.
    ///
    /// `config` + `jitter` are shared across all nodes (the spec
    /// assumes operators set them uniformly).
    pub fn new(nodes: Vec<(&str, Role)>, config: ElectConfig, jitter: ElectJitter) -> Self {
        let peer_ids: Vec<String> = nodes.iter().map(|(n, _)| (*n).to_string()).collect();
        let map = nodes
            .into_iter()
            .map(|(id, role)| {
                let elector = Elector::new(
                    id,
                    peer_ids.clone(),
                    format!("10.0.0.{}:6004", first_byte_addr(id)),
                    role,
                    config,
                    jitter.clone(),
                );
                (id.to_string(), SimNode { elector, alive: true })
            })
            .collect();
        Self {
            nodes: map,
            partitions: Vec::new(),
        }
    }

    /// Kill a node — stops its ticks + drops every message routed
    /// to it. Used to simulate primary loss / replica loss /
    /// process crash.
    pub fn kill_node(&mut self, id: &str) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.alive = false;
        }
    }

    /// Revive a previously-killed node. Its election state is
    /// preserved across the kill (matches "process restart with
    /// data dir intact"). For "process restart with data dir
    /// wiped", build a fresh sim with the same node id.
    pub fn revive_node(&mut self, id: &str) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.alive = true;
        }
    }

    /// Partition two nodes — messages between them are dropped in
    /// BOTH directions until [`Sim::heal_partition`] is called.
    /// Adding the same pair twice is idempotent.
    pub fn partition(&mut self, a: &str, b: &str) {
        let p = canonical_pair(a, b);
        if !self.partitions.contains(&p) {
            self.partitions.push(p);
        }
    }

    /// Heal a previously-installed partition.
    pub fn heal_partition(&mut self, a: &str, b: &str) {
        let p = canonical_pair(a, b);
        self.partitions.retain(|x| x != &p);
    }

    /// Set this node's `repl_offset` — the kevy-server adapter
    /// would call `elector.set_repl_offset(N)` from the live
    /// replication source / runner; tests can do the same here.
    pub fn set_offset(&mut self, id: &str, offset: u64) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.elector.set_repl_offset(offset);
        }
    }

    /// Query a node's current role (`None` if the node was never
    /// registered).
    pub fn role(&self, id: &str) -> Option<Role> {
        self.nodes.get(id).map(|n| n.elector.role())
    }

    /// Query a node's currently-known primary (`None` if the node
    /// wasn't registered, OR if it has not yet learned a primary).
    pub fn current_primary(&self, id: &str) -> Option<&str> {
        self.nodes.get(id).and_then(|n| n.elector.current_primary())
    }

    /// Query a node's current epoch.
    pub fn epoch(&self, id: &str) -> Option<u64> {
        self.nodes.get(id).map(|n| n.elector.epoch())
    }

    /// One simulation round at `now`:
    ///
    /// 1. Tick every alive node — collect every outbound message
    ///    produced.
    /// 2. Route every collected message to its target(s), dropping
    ///    messages whose `(sender, target)` pair is partitioned or
    ///    whose target is dead.
    /// 3. Feed each routed message to the target's `on_message` +
    ///    collect any second-round outbound (e.g. an OFFER ACCEPTed
    ///    on the same round) — route those too, with the same
    ///    drop rules. (This handles the spec's "ACCEPT triggers
    ///    nothing immediately; tick later emits ANNOUNCE" rhythm,
    ///    but if any handler emits messages we still route them.)
    pub fn tick_all(&mut self, now: Instant) {
        // Phase 1: gather every alive node's tick output.
        let mut pending: Vec<(String, Outbound)> = Vec::new();
        for (id, node) in &mut self.nodes {
            if !node.alive {
                continue;
            }
            for out in node.elector.tick(now) {
                pending.push((id.clone(), out));
            }
        }
        self.route(pending, now);
    }

    fn route(&mut self, pending: Vec<(String, Outbound)>, now: Instant) {
        // BFS-ish: route the first batch, collect any second-round
        // outputs, route those, and so on until no more remain.
        let mut queue: Vec<(String, Outbound)> = pending;
        while let Some((from, out)) = queue.pop() {
            // Expand broadcast.
            let targets: Vec<String> = if out.to == Outbound::BROADCAST {
                self.nodes
                    .keys()
                    .filter(|k| *k != &from)
                    .cloned()
                    .collect()
            } else {
                vec![out.to.clone()]
            };
            for target in targets {
                if self.is_partitioned(&from, &target) {
                    continue;
                }
                let Some(target_node) = self.nodes.get_mut(&target) else {
                    continue;
                };
                if !target_node.alive {
                    continue;
                }
                let new_outs = target_node.elector.on_message(&from, out.msg.clone(), now);
                for n in new_outs {
                    queue.push((target.clone(), n));
                }
            }
        }
    }

    fn is_partitioned(&self, a: &str, b: &str) -> bool {
        let p = canonical_pair(a, b);
        self.partitions.contains(&p)
    }
}

fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn first_byte_addr(id: &str) -> u32 {
    // Stable per-id last-byte for synthetic addresses (tests don't
    // care about uniqueness, this is just for legibility in test
    // logs). Cap at 254.
    (id.as_bytes().last().copied().unwrap_or(b'1') as u32).min(254)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ElectConfig {
        ElectConfig {
            hb_interval: Duration::from_millis(10),
            down_after: Duration::from_millis(100),
            election_timeout: Duration::from_millis(80),
            election_backoff: Duration::from_millis(20),
            election_backoff_jitter: Duration::from_millis(0),
        }
    }

    fn jitter() -> ElectJitter {
        ElectJitter::Fixed(Duration::from_millis(0))
    }

    /// T1.5.12 — 3-node primary kill. Quorum (2/3) promotes a
    /// replica within `down_after + election_timeout`.
    #[test]
    fn three_node_primary_kill_promotes_replica() {
        let mut sim = Sim::new(
            vec![("a", Role::Primary), ("b", Role::Replica), ("c", Role::Replica)],
            cfg(),
            jitter(),
        );
        let t0 = Instant::now();
        // Bootstrap announce: a is primary, b + c learn it.
        sim.tick_all(t0);
        for id in ["b", "c"] {
            // Receive at least one HB before the kill so they know a is current primary.
            assert!(sim.role(id) == Some(Role::Replica));
        }
        // Force-known-primary on b + c (the bootstrap path
        // currently doesn't emit an ANNOUNCE at t0; the replicas
        // don't know who the primary is until HBs accumulate +
        // a real one ANNOUNCEs). Set the current_primary directly
        // for the test scenario.
        for id in ["b", "c"] {
            sim.nodes.get_mut(id).unwrap().elector.force_known_primary("a");
        }
        // Kill a; advance time past down_after.
        sim.kill_node("a");
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
        }
        // One of b/c must be the new primary.
        let b_role = sim.role("b").unwrap();
        let c_role = sim.role("c").unwrap();
        assert!(
            b_role == Role::Primary || c_role == Role::Primary,
            "no replica promoted (b={b_role:?}, c={c_role:?})",
        );
        // The other became a replica acknowledging the new
        // primary.
        let (winner, loser) = if b_role == Role::Primary { ("b", "c") } else { ("c", "b") };
        assert_eq!(sim.current_primary(loser), Some(winner));
    }

    /// T1.5.13 — 5-node: kill primary + one replica; remaining 3
    /// (quorum) still promote.
    #[test]
    fn five_node_kill_primary_and_one_replica_still_promotes() {
        let mut sim = Sim::new(
            vec![
                ("a", Role::Primary),
                ("b", Role::Replica),
                ("c", Role::Replica),
                ("d", Role::Replica),
                ("e", Role::Replica),
            ],
            cfg(),
            jitter(),
        );
        let t0 = Instant::now();
        sim.tick_all(t0);
        for id in ["b", "c", "d", "e"] {
            sim.nodes.get_mut(id).unwrap().elector.force_known_primary("a");
        }
        sim.kill_node("a");
        sim.kill_node("e");
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
        }
        // One of b/c/d must be the new primary.
        let alive = ["b", "c", "d"];
        let promoted = alive.iter().filter(|id| sim.role(id) == Some(Role::Primary)).count();
        assert_eq!(promoted, 1, "exactly one promotion expected");
    }

    /// T1.5.14 — 3-node split-brain: partition primary from 2
    /// replicas. Minority (primary alone) cannot promote (no
    /// quorum — primary stays primary by inertia, but the *new*
    /// would-be primary on the majority side promotes).
    #[test]
    fn three_node_partition_majority_promotes_minority_stays() {
        let mut sim = Sim::new(
            vec![("a", Role::Primary), ("b", Role::Replica), ("c", Role::Replica)],
            cfg(),
            jitter(),
        );
        let t0 = Instant::now();
        sim.tick_all(t0);
        for id in ["b", "c"] {
            sim.nodes.get_mut(id).unwrap().elector.force_known_primary("a");
        }
        // Partition a from {b, c}. a still thinks it's primary;
        // b + c can't hear it → DOWN; they elect among themselves.
        sim.partition("a", "b");
        sim.partition("a", "c");
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
        }
        // Exactly one of b/c becomes primary on the majority side.
        let promoted_majority = ["b", "c"]
            .iter()
            .filter(|id| sim.role(id) == Some(Role::Primary))
            .count();
        assert_eq!(promoted_majority, 1, "majority must promote exactly one");
        // a (minority of one) cannot reach quorum — its role
        // doesn't matter for split-brain protection because no
        // peer accepts writes from it anyway. It MIGHT have tried
        // candidacy + timed out; either way it's not Primary OR
        // it is Primary-by-inertia (stale).
        // The KEY split-brain property is: minority side never
        // gets a higher epoch quorum, so its writes won't be
        // accepted by the majority on heal.
        let majority_epoch = sim.epoch("b").unwrap().max(sim.epoch("c").unwrap());
        let minority_epoch = sim.epoch("a").unwrap();
        assert!(
            majority_epoch > minority_epoch,
            "majority epoch ({majority_epoch}) must exceed minority ({minority_epoch})",
        );
    }

    /// T1.5.15 — dueling promotion: tie offsets, deterministic by
    /// lowest node-id. With two simultaneous candidates a + b at
    /// the same offset, a (a < b lexicographic) wins.
    #[test]
    fn dueling_candidates_lowest_node_id_wins() {
        let mut sim = Sim::new(
            vec![("a", Role::Primary), ("b", Role::Replica), ("c", Role::Replica)],
            cfg(),
            jitter(),
        );
        let t0 = Instant::now();
        sim.tick_all(t0);
        for id in ["b", "c"] {
            sim.nodes.get_mut(id).unwrap().elector.force_known_primary("a");
        }
        // Same offset on b + c — they tie if a dies.
        sim.set_offset("b", 100);
        sim.set_offset("c", 100);
        sim.kill_node("a");
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
        }
        // b (lower node-id) wins.
        assert_eq!(sim.role("b"), Some(Role::Primary));
        assert_eq!(sim.role("c"), Some(Role::Replica));
        assert_eq!(sim.current_primary("c"), Some("b"));
    }

    /// T1.5.16 — old primary rejoin: partition + heal. The old
    /// primary (a) sees a higher epoch on rejoin and demotes
    /// cleanly. No double-write because a doesn't receive ACCEPTs
    /// from the majority while partitioned.
    #[test]
    fn old_primary_rejoin_demotes_cleanly() {
        let mut sim = Sim::new(
            vec![("a", Role::Primary), ("b", Role::Replica), ("c", Role::Replica)],
            cfg(),
            jitter(),
        );
        let t0 = Instant::now();
        sim.tick_all(t0);
        for id in ["b", "c"] {
            sim.nodes.get_mut(id).unwrap().elector.force_known_primary("a");
        }
        sim.partition("a", "b");
        sim.partition("a", "c");
        let mut t = t0;
        // Majority elects b or c.
        for _ in 0..30 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
        }
        let new_primary = ["b", "c"]
            .iter()
            .find(|id| sim.role(id) == Some(Role::Primary))
            .unwrap();
        let new_primary = new_primary.to_string();
        // Heal partition. a's HBs reach majority + vice versa.
        sim.heal_partition("a", "b");
        sim.heal_partition("a", "c");
        for _ in 0..20 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
        }
        // a demotes (sees higher epoch ANNOUNCE / HB from new primary).
        assert_eq!(sim.role("a"), Some(Role::Replica));
        assert_eq!(sim.current_primary("a"), Some(new_primary.as_str()));
    }

    /// T1.5.17 — N=2 degenerate: quorum = 2; either node down =
    /// locked. The test confirms: kill the primary in N=2, the
    /// survivor candidate cannot reach quorum (needs ACCEPT from
    /// the dead peer) → stays Replica indefinitely.
    #[test]
    fn n2_degenerate_primary_kill_locks_survivor() {
        let mut sim = Sim::new(
            vec![("a", Role::Primary), ("b", Role::Replica)],
            cfg(),
            jitter(),
        );
        let t0 = Instant::now();
        sim.tick_all(t0);
        sim.nodes.get_mut("b").unwrap().elector.force_known_primary("a");
        sim.kill_node("a");
        let mut t = t0;
        let mut ever_primary = false;
        for _ in 0..200 {
            t += Duration::from_millis(10);
            sim.tick_all(t);
            if sim.role("b") == Some(Role::Primary) {
                ever_primary = true;
                break;
            }
        }
        // b NEVER reaches Primary — N=2 locks on either-down. b
        // can oscillate between Replica + Candidate + backoff
        // forever, but it never gets the quorum=2 ACCEPTs.
        assert!(!ever_primary, "b promoted in N=2 with dead peer — split-brain risk!");
    }
}
