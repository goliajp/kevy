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
    key: Vec<u8>,
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
    entry: Option<u32>,
    live: u64,
    /// Deterministic level generator (splitmix — no wall clock).
    seed: u64,
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
        Self { params, dim, nodes: Vec::new(), by_key: HashMap::new(), entry: None, live: 0, seed: 0x9E3779B97F4A7C15 }
    }

    /// Declared dimensionality.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Insert or replace `key`'s vector (`None` = remove). Replace =
    /// tombstone old + insert new (RFC D5).
    pub fn apply(&mut self, key: &[u8], vector: Option<Vec<f32>>) {
        if let Some(&id) = self.by_key.get(key) {
            let node = &mut self.nodes[id as usize];
            if !node.dead {
                node.dead = true;
                self.live -= 1;
            }
            self.by_key.remove(key);
            if self.entry == Some(id) {
                self.entry = self.pick_entry();
            }
        }
        let Some(mut v) = vector else { return };
        if v.len() != self.dim {
            return;
        }
        self.params.distance.prepare(&mut v);
        self.insert_prepared(key.to_vec(), v);
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
        self.seed = self.seed.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        let u = (z >> 11) as f64 / (1u64 << 53) as f64;
        let ml = 1.0 / (self.params.m as f64).ln();
        (-u.max(1e-12).ln() * ml).floor() as usize
    }

    fn insert_prepared(&mut self, key: Vec<u8>, v: Vec<f32>) {
        let level = self.rand_level();
        let id = self.nodes.len() as u32;
        self.nodes.push(Node { key: key.clone(), vec: v, links: vec![Vec::new(); level + 1], dead: false });
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
            let chosen = self.select_diverse(&found, cap);
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

    fn search_layer_vec(&self, start: u32, tv: &[f32], layer: usize, ef: usize) -> Vec<(f32, u32)> {
        let mut visited: HashMap<u32, ()> = HashMap::new();
        let mut result: BinaryHeap<Far> = BinaryHeap::new(); // max-heap of best ef
        let mut frontier: BinaryHeap<std::cmp::Reverse<Far>> = BinaryHeap::new();
        let d0 = self.params.distance.eval(&self.nodes[start as usize].vec, tv);
        visited.insert(start, ());
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
                    if visited.insert(n, ()).is_some() {
                        continue;
                    }
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
    }

    /// Malkov Algorithm 4 (diversity heuristic): walk candidates by
    /// ascending distance; keep one only if it's closer to the node
    /// than to every already-kept neighbor. This preserves BRIDGE
    /// links to otherwise-isolated regions (an outlier's closest
    /// in-graph node keeps its back-edge — plain closest-K pruning
    /// disconnects it).
    fn select_diverse(&self, sorted: &[(f32, u32)], cap: usize) -> Vec<u32> {
        let mut kept: Vec<u32> = Vec::with_capacity(cap);
        for &(d, c) in sorted {
            if kept.len() == cap {
                break;
            }
            let cv = &self.nodes[c as usize].vec;
            let diverse = kept.iter().all(|&s| {
                d < self.params.distance.eval(&self.nodes[s as usize].vec, cv)
            });
            if diverse {
                kept.push(c);
            }
        }
        // backfill with the nearest skipped candidates if under cap
        if kept.len() < cap {
            for &(_, c) in sorted {
                if kept.len() == cap {
                    break;
                }
                if !kept.contains(&c) {
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
        let kept = self.select_diverse(&scored, cap);
        self.nodes[node as usize].links[layer] = kept;
    }

    /// k nearest LIVING vectors to `query` (raw form; prepared here).
    pub fn knn(&self, query: &[f32], k: usize) -> Vec<(Vec<u8>, f32)> {
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
        let ef = k.max(64);
        let found = self.search_layer_vec(cur, &q, 0, ef);
        found
            .into_iter()
            .filter(|&(_, n)| !self.nodes[n as usize].dead)
            .take(k)
            .map(|(d, n)| (self.nodes[n as usize].key.clone(), d))
            .collect()
    }

    /// Membership (living only).
    pub fn contains(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Counters (RFC D6).
    pub fn stats(&self) -> VectorStats {
        let links: u64 = self.nodes.iter().map(|n| n.links.iter().map(Vec::len).sum::<usize>() as u64).sum();
        let tombstones = self.nodes.len() as u64 - self.live;
        let bytes_vec = (self.dim * 4) as u64;
        let approx_bytes: u64 = self.nodes.len() as u64 * (bytes_vec + 40)
            + links * 8
            + self.live * 32;
        VectorStats {
            vectors: self.live,
            tombstones,
            links,
            approx_bytes,
            rebuild_recommended: self.nodes.len() > 0 && tombstones * 10 > self.nodes.len() as u64 * 3,
        }
    }

    /// Bounded rebuild: re-insert every living vector into a fresh
    /// graph (drops tombstones and their edges) — RFC D5.
    pub fn rebuild(&mut self) {
        let mut fresh = Hnsw::new(self.dim, self.params);
        fresh.seed = self.seed;
        for node in &self.nodes {
            if !node.dead {
                fresh.insert_prepared(node.key.clone(), node.vec.clone());
            }
        }
        *self = fresh;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(n: usize) -> Hnsw {
        // 2-d grid points — L2 neighbors are unambiguous
        let mut h = Hnsw::new(2, HnswParams { distance: Distance::L2, ..Default::default() });
        for i in 0..n {
            let (x, y) = ((i % 32) as f32, (i / 32) as f32);
            h.apply(format!("p{i:04}").as_bytes(), Some(vec![x, y]));
        }
        h
    }

    #[test]
    fn knn_exact_on_grid() {
        let h = grid(1024);
        // nearest to (5.1, 7.05) is p(7*32+5)=p0229, then p0197/p0230…
        let hits = h.knn(&[5.1, 7.05], 3);
        assert_eq!(hits[0].0, b"p0229".to_vec(), "{hits:?}");
        assert_eq!(hits.len(), 3);
        assert!(hits[0].1 < hits[1].1 || (hits[0].1 == hits[1].1));
    }

    #[test]
    fn tombstone_and_replace() {
        let mut h = grid(256);
        h.apply(b"p0000", None);
        assert!(!h.contains(b"p0000"));
        let hits = h.knn(&[0.0, 0.0], 1);
        assert_ne!(hits[0].0, b"p0000".to_vec(), "dead filtered");
        // replace moves the point
        h.apply(b"p0001", Some(vec![100.0, 100.0]));
        let hits = h.knn(&[100.0, 100.0], 1);
        assert_eq!(hits[0].0, b"p0001".to_vec());
        let st = h.stats();
        assert_eq!(st.vectors, 255);
        assert_eq!(st.tombstones, 2, "one delete + one replace");
    }

    #[test]
    fn recall_on_random_vectors() {
        // deterministic pseudo-random 64-d vectors; HNSW top-10 vs
        // brute force ground truth, recall must be ≥ 0.9
        let mut seed = 42u64;
        let mut rnd = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) as f32 - 0.5
        };
        let mut h = Hnsw::new(64, HnswParams::default());
        let mut all: Vec<(Vec<u8>, Vec<f32>)> = Vec::new();
        for i in 0..2000 {
            let v: Vec<f32> = (0..64).map(|_| rnd()).collect();
            let key = format!("v{i:04}").into_bytes();
            h.apply(&key, Some(v.clone()));
            all.push((key, v));
        }
        let mut hit = 0usize;
        let mut total = 0usize;
        for qi in 0..20 {
            let q: Vec<f32> = (0..64).map(|_| rnd()).collect();
            let got: Vec<Vec<u8>> = h.knn(&q, 10).into_iter().map(|(k, _)| k).collect();
            // brute force with the same metric incl. normalization
            let mut qq = q.clone();
            Distance::Cosine.prepare(&mut qq);
            let mut truth: Vec<(f32, &[u8])> = all
                .iter()
                .map(|(k, v)| {
                    let mut vv = v.clone();
                    Distance::Cosine.prepare(&mut vv);
                    (Distance::Cosine.eval(&vv, &qq), k.as_slice())
                })
                .collect();
            truth.sort_by(|a, b| a.0.total_cmp(&b.0));
            let want: Vec<&[u8]> = truth[..10].iter().map(|(_, k)| *k).collect();
            for w in &want {
                total += 1;
                if got.iter().any(|g| g == w) {
                    hit += 1;
                }
            }
            let _ = qi;
        }
        let recall = hit as f64 / total as f64;
        assert!(recall >= 0.9, "recall {recall}");
    }

    #[test]
    fn rebuild_drops_tombstones_preserves_answers() {
        let mut h = grid(512);
        for i in 0..200 {
            h.apply(format!("p{i:04}").as_bytes(), None);
        }
        assert!(h.stats().rebuild_recommended);
        let before = h.knn(&[20.0, 10.0], 5);
        h.rebuild();
        let st = h.stats();
        assert_eq!(st.tombstones, 0);
        assert_eq!(st.vectors, 312);
        let after = h.knn(&[20.0, 10.0], 5);
        assert_eq!(
            before.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            after.iter().map(|(k, _)| k).collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_and_dim_mismatch() {
        let h = Hnsw::new(4, HnswParams::default());
        assert!(h.knn(&[1.0, 2.0, 3.0, 4.0], 5).is_empty());
        let mut h = grid(16);
        h.apply(b"bad", Some(vec![1.0, 2.0, 3.0])); // wrong dim ignored
        assert!(!h.contains(b"bad"));
        assert!(h.knn(&[1.0], 5).is_empty(), "query dim mismatch");
    }
}
