//! HNSW graph: hierarchical layers, greedy descent +
//! beam search on layer 0, tombstone deletes filtered at search
//! time, bounded full rebuild by re-inserting the living.

use std::collections::HashMap;

use crate::params::{HnswParams, VectorStats};

struct Node {
    /// Every LIVING key whose vector is exactly this one (duplicate
    /// vectors under different keys collapse onto ONE graph node —
    /// fuzz-found rationale: one-node-per-key duplicate clusters
    /// larger than the link cap disconnect from the graph because
    /// every co-located edge ties in the diversity prune).
    keys: Vec<Vec<u8>>,
    vec: Vec<f32>,
    /// links[layer] = neighbor node ids.
    links: Vec<Vec<u32>>,
    dead: bool,
}

/// One shard's ANN graph for one index.
/// # Examples
///
/// Build an index, put three vectors in it, and ask for the nearest. The
/// query is the second vector exactly, so it must come back first at
/// distance zero.
///
/// ```
/// use kevy_vector::{Hnsw, HnswParams};
/// let mut idx = Hnsw::new(3, HnswParams::default());
/// idx.apply(b"a", Some(vec![1.0, 0.0, 0.0]));
/// idx.apply(b"b", Some(vec![0.0, 1.0, 0.0]));
/// idx.apply(b"c", Some(vec![0.0, 0.0, 1.0]));
///
/// let hits = idx.knn(&[0.0, 1.0, 0.0], 1, 16);
/// assert_eq!(hits.len(), 1);
/// assert_eq!(hits[0].0, b"b".to_vec());
/// assert!(hits[0].1 < 1e-6, "an exact match is distance zero, got {}", hits[0].1);
/// ```
///
/// `apply` with `None` removes, and the key stops being returned.
///
/// ```
/// use kevy_vector::{Hnsw, HnswParams};
/// let mut idx = Hnsw::new(2, HnswParams::default());
/// idx.apply(b"gone", Some(vec![1.0, 0.0]));
/// idx.apply(b"kept", Some(vec![0.0, 1.0]));
/// idx.apply(b"gone", None);
/// let keys: Vec<_> = idx.knn(&[1.0, 0.0], 5, 16).into_iter().map(|h| h.0).collect();
/// assert!(!keys.contains(&b"gone".to_vec()));
/// assert!(keys.contains(&b"kept".to_vec()));
/// ```
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
    /// Running Σ of every node's link-slot count —
    /// maintained at the link push / shrink sites so `stats()` never
    /// walks the graph.
    links_total: u64,
    /// Running tombstone count — dead nodes never revive outside
    /// `rebuild`, which starts from a fresh graph.
    tombstones: u64,
    /// Deterministic level generator (splitmix — no wall clock).
    seed: u64,
}

/// Bitwise identity of a prepared vector (`by_vec` map key).
fn vec_bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

impl Hnsw {
    /// Empty graph for `dim`-dimensional vectors.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let h = Hnsw::new(128, HnswParams::default());
    /// assert_eq!(h.dim(), 128);
    /// assert_eq!(h.stats().vectors, 0);
    /// ```
    pub fn new(dim: usize, params: HnswParams) -> Self {
        Self {
            params,
            dim,
            nodes: Vec::new(),
            by_key: HashMap::new(),
            by_vec: HashMap::new(),
            entry: None,
            live: 0,
            links_total: 0,
            tombstones: 0,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Declared dimensionality.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// // Fixed at declaration: a vector of another length is not indexed.
    /// let h = Hnsw::new(3, HnswParams::default());
    /// assert_eq!(h.dim(), 3);
    /// ```
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Insert or replace `key`'s vector (`None` = remove). Replace =
    /// detach old key (tombstone the node once keyless) + insert new.
    /// Keys sharing one exact vector share one graph node.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    ///
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// assert!(h.contains(b"a"));
    ///
    /// // `None` removes.
    /// h.apply(b"a", None);
    /// assert!(!h.contains(b"a"));
    ///
    /// // Keys sharing one EXACT vector share one graph node, so the
    /// // second costs no distance evaluations.
    /// h.apply(b"x", Some(vec![0.0, 1.0]));
    /// h.apply(b"y", Some(vec![0.0, 1.0]));
    /// assert_eq!(h.stats().vectors, 2);
    /// ```
    pub fn apply(&mut self, key: &[u8], vector: Option<Vec<f32>>) {
        if let Some(id) = self.by_key.remove(key) {
            let node = &mut self.nodes[id as usize];
            node.keys.retain(|k| k != key);
            self.live -= 1;
            if node.keys.is_empty() {
                node.dead = true;
                self.tombstones += 1;
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
                self.links_total += 2;
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
        let dropped = self.nodes[node as usize].links[layer].len() - kept.len();
        self.links_total -= dropped as u64;
        self.nodes[node as usize].links[layer] = kept;
    }

    /// k nearest LIVING vectors to `query` (raw form; prepared here).
    /// `ef` = query beam width (0 → the max(4k, 100) default); larger
    /// beams trade latency for recall — the canonical HNSW knob.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// h.apply(b"east", Some(vec![1.0, 0.0]));
    /// h.apply(b"north", Some(vec![0.0, 1.0]));
    ///
    /// // `ef` is the query beam width; 0 asks for the engine default.
    /// let hits = h.knn(&[0.9, 0.1], 1, 0);
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].0, b"east".to_vec(), "nearest first");
    ///
    /// // k is a ceiling, not a promise: an index holding fewer answers
    /// // with fewer rather than padding.
    /// assert_eq!(h.knn(&[1.0, 0.0], 10, 0).len(), 2);
    /// ```
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
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// assert!(!h.contains(b"a"));
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// assert!(h.contains(b"a"));
    /// ```
    pub fn contains(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Bounded rebuild: re-insert every living (key, vector) pair into
    /// a fresh graph (drops tombstones and their edges).
    /// Vectors are already prepared; `add_key` re-collapses duplicates.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// for i in 0..4u8 {
    ///     h.apply(&[b'k', i], Some(vec![f32::from(i), 1.0]));
    /// }
    /// h.apply(&[b'k', 0], None);
    /// assert_eq!(h.stats().tombstones, 1);
    ///
    /// // Compaction: the tombstones go, the living vectors stay, and
    /// // the surviving keys are still findable.
    /// h.rebuild();
    /// assert_eq!(h.stats().tombstones, 0);
    /// assert_eq!(h.stats().vectors, 3);
    /// assert!(h.contains(&[b'k', 1]));
    /// ```
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

#[path = "hnsw_search.rs"]
mod hnsw_search;

#[path = "hnsw_stats.rs"]
mod hnsw_stats;
