//! The beam search under [`Hnsw`] — the read half of the graph walk
//! (child module via `#[path]`, the `hnsw_stats.rs` house pattern),
//! split from `hnsw.rs` for the 500-LOC ceiling.
//!
//! The seam is direction: `hnsw.rs` CHANGES the graph (insert, link,
//! tombstone, rebuild) and this file WALKS it. `search_layer_vec` is the
//! hot body both halves lean on, and it lives with the walk.

use std::collections::BinaryHeap;

use super::Hnsw;

/// Max-heap entry by distance (candidate pruning pops farthest).
#[derive(PartialEq)]
struct Far(f32, u32);
impl Eq for Far {}
impl PartialOrd for Far {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Far {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0).then_with(|| self.1.cmp(&other.1))
    }
}

impl Hnsw {
    pub(super) fn greedy_at(&self, mut cur: u32, target: u32, layer: usize) -> u32 {
        let tv = &self.nodes[target as usize].vec;
        let mut best = self.params.distance.eval(&self.nodes[cur as usize].vec, tv);
        loop {
            let mut improved = false;
            if layer < self.nodes[cur as usize].links.len() {
                for &n in &self.nodes[cur as usize].links[layer] {
                    let d = self.params.distance.eval(&self.nodes[n as usize].vec, tv);
                    if d < best {
                        best = d;
                        cur = n;
                        improved = true;
                    }
                }
            }
            if !improved {
                return cur;
            }
        }
    }

    /// Beam search at one layer. `include_dead` keeps tombstones as
    /// ROUTING waypoints (their links still connect the graph);
    /// results always include them so the caller can filter.
    pub(super) fn search_layer(
        &self,
        start: u32,
        target: u32,
        layer: usize,
        ef: usize,
        _for_insert: bool,
    ) -> Vec<(f32, u32)> {
        let tv = &self.nodes[target as usize].vec;
        self.search_layer_vec(start, tv, layer, ef)
    }

    // LOC-WAIVER: per-query beam-search hot body (63% of EF16 KNN self-time; see comment below).
    pub(super) fn search_layer_vec(
        &self,
        start: u32,
        tv: &[f32],
        layer: usize,
        ef: usize,
    ) -> Vec<(f32, u32)> {
        // The visited set is the beam search's hottest structure —
        // perf-record put 63% of the EF16 KNN shape inside this fn,
        // with the std HashMap's SipHash showing as a distinct cost.
        // Classic hnswlib answer: an epoch-stamped visited pool —
        // membership is ONE u32 array read, reset is `epoch += 1`,
        // and the thread_local reuses the allocation across queries
        // (thread-per-core: shards never share a search).
        thread_local! {
            static VISITED: std::cell::RefCell<(Vec<u32>, u32)> =
                const { std::cell::RefCell::new((Vec::new(), 0)) };
        }
        VISITED.with(|cell| {
            let (stamps, epoch) = &mut *cell.borrow_mut();
            if stamps.len() < self.nodes.len() {
                stamps.resize(self.nodes.len(), 0);
            }
            *epoch = epoch.wrapping_add(1);
            if *epoch == 0 {
                stamps.fill(0);
                *epoch = 1;
            }
            let epoch = *epoch;
            let mut result: BinaryHeap<Far> = BinaryHeap::with_capacity(ef + 1);
            let mut frontier: BinaryHeap<std::cmp::Reverse<Far>> =
                BinaryHeap::with_capacity(ef * 2);
            let d0 = self.params.distance.eval(&self.nodes[start as usize].vec, tv);
            stamps[start as usize] = epoch;
            result.push(Far(d0, start));
            frontier.push(std::cmp::Reverse(Far(d0, start)));
            while let Some(std::cmp::Reverse(Far(d, node))) = frontier.pop() {
                if result.len() >= ef
                    && let Some(worst) = result.peek()
                    && d > worst.0
                {
                    break;
                }
                if layer < self.nodes[node as usize].links.len() {
                    for &n in &self.nodes[node as usize].links[layer] {
                        if stamps[n as usize] == epoch {
                            continue;
                        }
                        stamps[n as usize] = epoch;
                        let dn = self.params.distance.eval(&self.nodes[n as usize].vec, tv);
                        if result.len() < ef || dn < result.peek().expect("nonempty").0 {
                            result.push(Far(dn, n));
                            if result.len() > ef {
                                result.pop();
                            }
                            frontier.push(std::cmp::Reverse(Far(dn, n)));
                        }
                    }
                }
            }
            let mut out: Vec<(f32, u32)> = result.into_iter().map(|Far(d, n)| (d, n)).collect();
            out.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            out
        })
    }

    /// Malkov Algorithm 4 (diversity heuristic): walk candidates by
    /// ascending distance; keep one unless an already-kept neighbor is
    /// STRICTLY closer to it than the node is. This preserves BRIDGE
    /// links to otherwise-isolated regions (an outlier's closest
    /// in-graph node keeps its back-edge — plain closest-K pruning
    /// disconnects it).
    ///
    /// Duplicate handling (fuzz-found: recall@10 = 0.8
    /// under an exhaustive beam — duplicate vectors under different
    /// keys are legal in production):
    ///
    /// * ties are kept (`<=`, matching hnswlib): with a strict `<`,
    ///   any candidate tying a kept neighbor — always the case once a
    ///   kept neighbor duplicates the node — lost, degenerating the
    ///   prune to closest-K, which drops bridges;
    /// * candidates co-located WITH the node collapse to ONE
    ///   representative edge (they tie everything, so without the cap
    ///   a duplicate cluster larger than `cap` fills every slot and
    ///   the cluster's bridges to the rest of the graph are all
    ///   pruned — the cluster becomes an island); the backfill also
    ///   prefers non-co-located candidates for the same reason.
    pub(super) fn select_diverse(
        &self,
        sorted: &[(f32, u32)],
        cap: usize,
        node_vec: &[f32],
    ) -> Vec<u32> {
        // Co-location = vector equality, NOT distance 0 (ip distance
        // of co-located vectors is -|v|², and 0 for orthogonal ones).
        let co = |c: u32| self.nodes[c as usize].vec == node_vec;
        let mut kept: Vec<u32> = Vec::with_capacity(cap);
        let mut have_twin = false;
        for &(d, c) in sorted {
            if kept.len() == cap {
                break;
            }
            if co(c) {
                if !have_twin {
                    have_twin = true;
                    kept.push(c);
                }
                continue;
            }
            let cv = &self.nodes[c as usize].vec;
            let diverse = kept
                .iter()
                .all(|&s| d <= self.params.distance.eval(&self.nodes[s as usize].vec, cv));
            if diverse {
                kept.push(c);
            }
        }
        // Backfill with the nearest skipped candidates if under cap —
        // non-co-located first (bridges), co-located twins last.
        for pass in [false, true] {
            for &(_, c) in sorted {
                if kept.len() == cap {
                    return kept;
                }
                if (pass || !co(c)) && !kept.contains(&c) {
                    kept.push(c);
                }
            }
        }
        kept
    }
}
