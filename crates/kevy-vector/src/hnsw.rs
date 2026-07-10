//! HNSW graph (RFC D2/D5): hierarchical layers, greedy descent +
//! beam search on layer 0, tombstone deletes filtered at search
//! time, bounded full rebuild by re-inserting the living.

use std::collections::{BinaryHeap, HashMap};

use crate::dist::Distance;

/// Construction/search parameters (immutable once built — RFC D2).
#[derive(Debug, Clone, Copy)]
pub struct HnswParams {
    /// Max bidirectional links per node per layer (layer 0 gets 2M).
    pub m: usize,
    /// Construction beam width.
    pub ef_construction: usize,
    /// Metric.
    pub distance: Distance,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self { m: 16, ef_construction: 200, distance: Distance::Cosine }
    }
}

/// Sizing counters (RFC D6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorStats {
    /// Living vectors.
    pub vectors: u64,
    /// Tombstoned nodes still in the graph.
    pub tombstones: u64,
    /// Total graph links.
    pub links: u64,
    /// Approximate heap bytes.
    pub approx_bytes: u64,
    /// 1 when tombstones exceed the rebuild threshold (30%).
    pub rebuild_recommended: bool,
}

struct Node {
    /// Every LIVING key whose vector is exactly this one (duplicate
    /// vectors under different keys collapse onto ONE graph node —
    /// fuzz finding 2026-07-10: one-node-per-key duplicate clusters
    /// larger than the link cap disconnect from the graph because
    /// every co-located edge ties in the diversity prune).
    keys: Vec<Vec<u8>>,
    vec: Vec<f32>,
    /// links[layer] = neighbor node ids.
    links: Vec<Vec<u32>>,
    dead: bool,
}

/// One shard's ANN graph for one index.
pub struct Hnsw {
    params: HnswParams,
    dim: usize,
    nodes: Vec<Node>,
    by_key: HashMap<Vec<u8>, u32>,
    /// Prepared-vector bits → living node holding that exact vector
    /// (the duplicate-collapse index; bitwise equality, so -0.0/0.0
    /// stay distinct nodes — harmless, the tie-keeping prune covers
    /// sub-cap co-located pairs).
    by_vec: HashMap<Vec<u32>, u32>,
    entry: Option<u32>,
    /// Living KEYS (≥ living nodes when duplicates are collapsed).
    live: u64,
    /// Deterministic level generator (splitmix — no wall clock).
    seed: u64,
}

/// Bitwise identity of a prepared vector (`by_vec` map key).
fn vec_bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

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
    /// Empty graph for `dim`-dimensional vectors.
    pub fn new(dim: usize, params: HnswParams) -> Self {
        Self {
            params,
            dim,
            nodes: Vec::new(),
            by_key: HashMap::new(),
            by_vec: HashMap::new(),
            entry: None,
            live: 0,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Declared dimensionality.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Insert or replace `key`'s vector (`None` = remove). Replace =
    /// detach old key (tombstone the node once keyless) + insert new
    /// (RFC D5). Keys sharing one exact vector share one graph node.
    pub fn apply(&mut self, key: &[u8], vector: Option<Vec<f32>>) {
        if let Some(id) = self.by_key.remove(key) {
            let node = &mut self.nodes[id as usize];
            node.keys.retain(|k| k != key);
            self.live -= 1;
            if node.keys.is_empty() {
                node.dead = true;
                self.by_vec.remove(&vec_bits(&node.vec));
                if self.entry == Some(id) {
                    self.entry = self.pick_entry();
                }
            }
        }
        let Some(mut v) = vector else { return };
        if v.len() != self.dim {
            return;
        }
        self.params.distance.prepare(&mut v);
        self.add_key(key.to_vec(), v);
    }

    /// Attach a (key, PREPARED vector) pair: onto the living node
    /// already holding that exact vector, or as a fresh graph node.
    fn add_key(&mut self, key: Vec<u8>, v: Vec<f32>) {
        if let Some(&id) = self.by_vec.get(&vec_bits(&v)) {
            self.nodes[id as usize].keys.push(key.clone());
            self.by_key.insert(key, id);
            self.live += 1;
            return;
        }
        self.insert_prepared(key, v);
    }

    fn pick_entry(&self) -> Option<u32> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| !n.dead)
            .max_by_key(|(_, n)| n.links.len())
            .map(|(i, _)| i as u32)
    }

    fn rand_level(&mut self) -> usize {
        // splitmix64 → uniform in (0,1) → geometric with 1/ln(M)
        self.seed = self.seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let u = (z >> 11) as f64 / (1u64 << 53) as f64;
        let ml = 1.0 / (self.params.m as f64).ln();
        (-u.max(1e-12).ln() * ml).floor() as usize
    }

    fn insert_prepared(&mut self, key: Vec<u8>, v: Vec<f32>) {
        let level = self.rand_level();
        let id = self.nodes.len() as u32;
        self.by_vec.insert(vec_bits(&v), id);
        self.nodes.push(Node {
            keys: vec![key.clone()],
            vec: v,
            links: vec![Vec::new(); level + 1],
            dead: false,
        });
        self.by_key.insert(key, id);
        self.live += 1;
        let Some(mut cur) = self.entry else {
            self.entry = Some(id);
            return;
        };
        let top = (self.nodes[cur as usize].links.len() - 1) as i32;
        // greedy descent above the node's level
        for layer in ((level as i32 + 1)..=top).rev() {
            cur = self.greedy_at(cur, id, layer as usize);
        }
        // beam insert on the node's layers
        for layer in (0..=level.min(top.max(0) as usize)).rev() {
            let found = self.search_layer(cur, id, layer, self.params.ef_construction, true);
            let cap = if layer == 0 { self.params.m * 2 } else { self.params.m };
            let chosen = self.select_diverse(&found, cap, &self.nodes[id as usize].vec);
            for &n in &chosen {
                self.nodes[id as usize].links[layer].push(n);
                self.nodes[n as usize].links[layer].push(id);
                self.shrink(n, layer, cap);
            }
            if let Some(&(_, first)) = found.first() {
                cur = first;
            }
        }
        // a new top-level node becomes the entry
        if level as i32 > top {
            self.entry = Some(id);
        }
    }

    fn greedy_at(&self, mut cur: u32, target: u32, layer: usize) -> u32 {
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
    fn search_layer(&self, start: u32, target: u32, layer: usize, ef: usize, _for_insert: bool) -> Vec<(f32, u32)> {
        let tv = &self.nodes[target as usize].vec;
        self.search_layer_vec(start, tv, layer, ef)
    }

    // LOC-WAIVER: per-query beam-search hot body (63% of EF16 KNN self-time; see comment below).
    fn search_layer_vec(&self, start: u32, tv: &[f32], layer: usize, ef: usize) -> Vec<(f32, u32)> {
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
    /// Duplicate handling (fuzz finding 2026-07-10, recall@10 = 0.8
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
    fn select_diverse(&self, sorted: &[(f32, u32)], cap: usize, node_vec: &[f32]) -> Vec<u32> {
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
            let diverse = kept.iter().all(|&s| {
                d <= self.params.distance.eval(&self.nodes[s as usize].vec, cv)
            });
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

    fn shrink(&mut self, node: u32, layer: usize, cap: usize) {
        if self.nodes[node as usize].links[layer].len() <= cap {
            return;
        }
        let nv = &self.nodes[node as usize].vec;
        let mut scored: Vec<(f32, u32)> = self.nodes[node as usize].links[layer]
            .iter()
            .map(|&n| (self.params.distance.eval(&self.nodes[n as usize].vec, nv), n))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        scored.dedup_by_key(|e| e.1);
        let kept = self.select_diverse(&scored, cap, &self.nodes[node as usize].vec);
        self.nodes[node as usize].links[layer] = kept;
    }

    /// k nearest LIVING vectors to `query` (raw form; prepared here).
    /// `ef` = query beam width (0 → the max(4k, 100) default); larger
    /// beams trade latency for recall — the canonical HNSW knob.
    pub fn knn(&self, query: &[f32], k: usize, ef: usize) -> Vec<(Vec<u8>, f32)> {
        let Some(entry) = self.entry else { return Vec::new() };
        if query.len() != self.dim {
            return Vec::new();
        }
        let mut q = query.to_vec();
        self.params.distance.prepare(&mut q);
        let mut cur = entry;
        let top = self.nodes[cur as usize].links.len().saturating_sub(1);
        for layer in (1..=top).rev() {
            loop {
                let cv = &self.nodes[cur as usize].vec;
                let mut best = self.params.distance.eval(cv, &q);
                let mut next = cur;
                if layer < self.nodes[cur as usize].links.len() {
                    for &n in &self.nodes[cur as usize].links[layer] {
                        let d = self.params.distance.eval(&self.nodes[n as usize].vec, &q);
                        if d < best {
                            best = d;
                            next = n;
                        }
                    }
                }
                if next == cur {
                    break;
                }
                cur = next;
            }
        }
        // Recall grows with beam width (measured on a dense 20k
        // cluster @128d: ef 64 → 0.67 recall@10, 100 → 0.77); the
        // default floor suits easy corpora, hard ones pass EF.
        let ef = if ef == 0 { (k * 4).max(100) } else { ef.max(k) };
        let found = self.search_layer_vec(cur, &q, 0, ef);
        self.expand_living(found, k)
    }

    /// Expand collapsed duplicates: one graph node answers for every
    /// living key sharing its vector (all at the node's distance).
    fn expand_living(&self, found: Vec<(f32, u32)>, k: usize) -> Vec<(Vec<u8>, f32)> {
        let mut out: Vec<(Vec<u8>, f32)> = Vec::with_capacity(k);
        for (d, n) in found {
            let node = &self.nodes[n as usize];
            if node.dead {
                continue;
            }
            for key in &node.keys {
                if out.len() == k {
                    return out;
                }
                out.push((key.clone(), d));
            }
        }
        out
    }

    /// Membership (living only).
    pub fn contains(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Counters (RFC D6).
    pub fn stats(&self) -> VectorStats {
        let links: u64 = self.nodes.iter().map(|n| n.links.iter().map(Vec::len).sum::<usize>() as u64).sum();
        let tombstones = self.nodes.iter().filter(|n| n.dead).count() as u64;
        let bytes_vec = (self.dim * 4) as u64;
        let approx_bytes: u64 = self.nodes.len() as u64 * (bytes_vec + 40)
            + links * 8
            + self.live * 32;
        VectorStats {
            vectors: self.live,
            tombstones,
            links,
            approx_bytes,
            rebuild_recommended: !self.nodes.is_empty() && tombstones * 10 > self.nodes.len() as u64 * 3,
        }
    }

    /// Bounded rebuild: re-insert every living (key, vector) pair into
    /// a fresh graph (drops tombstones and their edges) — RFC D5.
    /// Vectors are already prepared; `add_key` re-collapses duplicates.
    pub fn rebuild(&mut self) {
        let mut fresh = Hnsw::new(self.dim, self.params);
        fresh.seed = self.seed;
        for node in &self.nodes {
            if !node.dead {
                for key in &node.keys {
                    fresh.add_key(key.clone(), node.vec.clone());
                }
            }
        }
        *self = fresh;
    }
}

#[cfg(test)]
#[path = "hnsw_tests.rs"]
mod tests;
