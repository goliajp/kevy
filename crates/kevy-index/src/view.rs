//! v2.6 — views: named composition trees over declared indexes
//! (RFC 2026-07-04, LOCKED).
//!
//! Pure logic: [`ViewSpec`] (the declaration), [`eval_tree`] (the
//! virtual-mode evaluator over segment closures), and
//! [`MaterializedSet`] (the incremental ordered result set with the
//! bounded top-K discipline). The runtime supplies segment access and
//! wires maintenance to its write hook — nothing here does I/O.
//!
//! Locked structural rules: components are NAMED indexes (leaves carry
//! a shape; the view layer holds no predicates of its own); a view
//! stores MEMBERSHIP + ORDER only (never field values); AND/OR
//! subtrees may be re-ordered by the engine (DIFF is fixed
//! left-right).

use crate::segment::Segment;
use crate::value::IndexValue;

/// One leaf: a declared index + the shape it contributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    /// Index name (resolved by the runtime).
    pub index: Vec<u8>,
    /// Inclusive bounds (EQ = same min/max), already coerced to the
    /// index's type by the runtime at CREATE time.
    pub min: IndexValue,
    /// Upper bound.
    pub max: IndexValue,
}

/// The composition tree. Depth ≤ 3, leaves ≤ 4 (declarative caps,
/// enforced at CREATE).
#[derive(Debug, Clone, PartialEq)]
pub enum Tree {
    /// A single index shape.
    Leaf(Leaf),
    /// Intersection.
    And(Box<Tree>, Box<Tree>),
    /// Union.
    Or(Box<Tree>, Box<Tree>),
    /// Left minus right (NOT commutative — order is fixed).
    Diff(Box<Tree>, Box<Tree>),
}

impl Tree {
    /// Number of leaves.
    pub fn leaves(&self) -> usize {
        match self {
            Tree::Leaf(_) => 1,
            Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => a.leaves() + b.leaves(),
        }
    }

    /// Depth (a leaf is 1).
    pub fn depth(&self) -> usize {
        match self {
            Tree::Leaf(_) => 1,
            Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => 1 + a.depth().max(b.depth()),
        }
    }

    /// Visit every leaf.
    pub fn each_leaf<F: FnMut(&Leaf)>(&self, f: &mut F) {
        match self {
            Tree::Leaf(l) => f(l),
            Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => {
                a.each_leaf(f);
                b.each_leaf(f);
            }
        }
    }
}

/// View mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Evaluate the tree at query time.
    Virtual,
    /// Maintain an incremental result set; `top_k = 0` = unbounded.
    Materialized {
        /// Bounded size (0 = keep every member).
        top_k: u32,
    },
}

/// A declared view.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSpec {
    /// Catalog name.
    pub name: Vec<u8>,
    /// The composition.
    pub tree: Tree,
    /// Index whose coerced value orders the view (a row absent from
    /// this index is excluded — declaratively, counted).
    pub order_by: Vec<u8>,
    /// Descending order?
    pub desc: bool,
    /// Virtual or materialized.
    pub mode: ViewMode,
    /// Optional `VIA` hydration byte-template (`{key}` / `{key.N}`
    /// placeholders; pure dereference, one template hop).
    pub via: Option<Vec<u8>>,
}

/// Declarative caps (RFC §1).
pub const MAX_TREE_DEPTH: usize = 3;
/// Max leaves per tree.
pub const MAX_TREE_LEAVES: usize = 4;

impl ViewSpec {
    /// Validate the structural caps.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.tree.depth() > MAX_TREE_DEPTH {
            return Err("ERR view tree deeper than 3");
        }
        if self.tree.leaves() > MAX_TREE_LEAVES {
            return Err("ERR view tree has more than 4 leaves");
        }
        Ok(())
    }
}

/// Evaluate `tree` against one shard's segments: `seg` resolves an
/// index name to its [`Segment`] (None = unknown index → empty leaf —
/// the runtime validates names at CREATE, so this is defensive).
/// Returns the member keys (unordered set semantics).
pub fn eval_tree<'a>(
    tree: &Tree,
    seg: &impl Fn(&[u8]) -> Option<&'a Segment>,
) -> Vec<Vec<u8>> {
    match tree {
        Tree::Leaf(l) => match seg(&l.index) {
            Some(s) => {
                let (hits, _) = s.range(&l.min, &l.max, None, usize::MAX);
                hits.into_iter().map(|(k, _)| k).collect()
            }
            None => Vec::new(),
        },
        Tree::And(a, b) => {
            // Engine may re-order (locked clause): drive the smaller
            // side, probe the larger.
            let (xa, xb) = (eval_tree(a, seg), eval_tree(b, seg));
            let (mut drive, probe) = if xa.len() <= xb.len() { (xa, xb) } else { (xb, xa) };
            let set: std::collections::HashSet<&[u8]> =
                probe.iter().map(Vec::as_slice).collect();
            drive.retain(|k| set.contains(k.as_slice()));
            drive
        }
        Tree::Or(a, b) => {
            let mut xa = eval_tree(a, seg);
            xa.extend(eval_tree(b, seg));
            xa.sort();
            xa.dedup();
            xa
        }
        Tree::Diff(a, b) => {
            let mut xa = eval_tree(a, seg);
            let xb = eval_tree(b, seg);
            let set: std::collections::HashSet<&[u8]> = xb.iter().map(Vec::as_slice).collect();
            xa.retain(|k| !set.contains(k.as_slice()));
            xa
        }
    }
}

/// Re-evaluate ONE key's membership (the materialized write hook):
/// every leaf is a point probe via the segment's reverse map.
pub fn key_in_tree<'a>(
    tree: &Tree,
    key: &[u8],
    seg: &impl Fn(&[u8]) -> Option<&'a Segment>,
) -> bool {
    match tree {
        Tree::Leaf(l) => seg(&l.index)
            .and_then(|s| s.verify_entry(key))
            .is_some_and(|v| *v >= l.min && *v <= l.max),
        Tree::And(a, b) => key_in_tree(a, key, seg) && key_in_tree(b, key, seg),
        Tree::Or(a, b) => key_in_tree(a, key, seg) || key_in_tree(b, key, seg),
        Tree::Diff(a, b) => key_in_tree(a, key, seg) && !key_in_tree(b, key, seg),
    }
}

/// One shard's materialized result set: ordered `(order_value, key)`
/// members with the bounded top-K discipline (keep `K + Δ` where
/// `Δ = K/4`; underflow requests a local rebuild from the base
/// indexes — RFC §2).
#[derive(Debug, Default)]
pub struct MaterializedSet {
    set: std::collections::BTreeSet<(IndexValue, Vec<u8>)>,
    back: std::collections::HashMap<Vec<u8>, IndexValue>,
    /// 0 = unbounded.
    top_k: u32,
    /// Members excluded because they're absent from the order index.
    pub order_excluded: u64,
}

impl MaterializedSet {
    /// New set with the declared bound (0 = unbounded).
    pub fn new(top_k: u32) -> Self {
        Self { top_k, ..Default::default() }
    }

    fn cap(&self) -> usize {
        if self.top_k == 0 {
            usize::MAX
        } else {
            (self.top_k + self.top_k / 4) as usize
        }
    }

    /// Apply one key's membership verdict + order value. Returns
    /// `true` if the set UNDERFLOWED below K after a removal (the
    /// caller must schedule a local rebuild).
    pub fn apply(&mut self, key: &[u8], member: bool, order: Option<IndexValue>) -> bool {
        if let Some(old) = self.back.remove(key) {
            self.set.remove(&(old, key.to_vec()));
        }
        match (member, order) {
            (true, Some(v)) => {
                self.back.insert(key.to_vec(), v.clone());
                self.set.insert((v, key.to_vec()));
                // bound: evict the WORST member past K+Δ
                if self.set.len() > self.cap()
                    && let Some(last) = self.set.iter().next_back().cloned()
                {
                    self.set.remove(&last);
                    self.back.remove(&last.1);
                }
                false
            }
            (true, None) => {
                self.order_excluded += 1;
                false
            }
            _ => {
                self.top_k != 0 && self.set.len() < self.top_k as usize
            }
        }
    }

    /// Ordered page (ascending; the runtime reverses for DESC).
    pub fn page(&self, after: Option<&(IndexValue, Vec<u8>)>, limit: usize) -> Vec<(IndexValue, Vec<u8>)> {
        let iter: Box<dyn Iterator<Item = &(IndexValue, Vec<u8>)>> = match after {
            Some(c) => Box::new(self.set.range((
                std::ops::Bound::Excluded(c.clone()),
                std::ops::Bound::Unbounded,
            ))),
            None => Box::new(self.set.iter()),
        };
        iter.take(limit).cloned().collect()
    }

    /// Member count.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Wipe (rebuild path).
    pub fn clear(&mut self) {
        self.set.clear();
        self.back.clear();
    }

    /// Approximate heap bytes (RFC §5 formula's measured side).
    pub fn approx_bytes(&self) -> u64 {
        self.set
            .iter()
            .map(|(v, k)| (v.approx_bytes() + k.len() + 48) as u64)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg_ab() -> (Segment, Segment) {
        let mut a = Segment::new();
        let mut b = Segment::new();
        for i in 0..10 {
            a.apply(format!("k{i}").as_bytes(), Some(IndexValue::I64(i)));
            if i % 2 == 0 {
                b.apply(format!("k{i}").as_bytes(), Some(IndexValue::Str(b"eng".to_vec())));
            }
        }
        (a, b)
    }

    fn leaf(idx: &str, min: IndexValue, max: IndexValue) -> Tree {
        Tree::Leaf(Leaf { index: idx.into(), min, max })
    }

    #[test]
    fn tree_eval_and_or_diff() {
        let (a, b) = seg_ab();
        let seg = |n: &[u8]| -> Option<&Segment> {
            match n {
                b"age" => Some(&a),
                b"dept" => Some(&b),
                _ => None,
            }
        };
        let age = leaf("age", IndexValue::I64(2), IndexValue::I64(7));
        let eng = leaf(
            "dept",
            IndexValue::Str(b"eng".to_vec()),
            IndexValue::Str(b"eng".to_vec()),
        );
        let and = Tree::And(Box::new(age.clone()), Box::new(eng.clone()));
        let mut got = eval_tree(&and, &seg);
        got.sort();
        assert_eq!(got, vec![b"k2".to_vec(), b"k4".to_vec(), b"k6".to_vec()]);

        let or = Tree::Or(Box::new(age.clone()), Box::new(eng.clone()));
        assert_eq!(eval_tree(&or, &seg).len(), 8, "2..=7 ∪ evens = 8");

        let diff = Tree::Diff(Box::new(age.clone()), Box::new(eng.clone()));
        let mut got = eval_tree(&diff, &seg);
        got.sort();
        assert_eq!(got, vec![b"k3".to_vec(), b"k5".to_vec(), b"k7".to_vec()]);

        // per-key membership mirrors set eval
        assert!(key_in_tree(&and, b"k4", &seg));
        assert!(!key_in_tree(&and, b"k3", &seg));
        assert!(key_in_tree(&diff, b"k5", &seg));
        assert!(!key_in_tree(&diff, b"k4", &seg));
    }

    #[test]
    fn caps_validate() {
        let l = leaf("a", IndexValue::I64(0), IndexValue::I64(1));
        let deep = Tree::And(
            Box::new(Tree::And(
                Box::new(Tree::And(Box::new(l.clone()), Box::new(l.clone()))),
                Box::new(l.clone()),
            )),
            Box::new(l.clone()),
        );
        let spec = ViewSpec {
            name: b"v".to_vec(),
            tree: deep,
            order_by: b"a".to_vec(),
            desc: false,
            mode: ViewMode::Virtual,
            via: None,
        };
        assert!(spec.validate().is_err(), "depth 4 rejected");
    }

    #[test]
    fn materialized_bounds_and_underflow() {
        let mut m = MaterializedSet::new(4); // cap = 4 + 1 = 5
        for i in 0..8 {
            let under = m.apply(
                format!("k{i}").as_bytes(),
                true,
                Some(IndexValue::I64(i)),
            );
            assert!(!under);
        }
        assert_eq!(m.len(), 5, "bounded at K+Δ");
        let page = m.page(None, 10);
        assert_eq!(page[0].1, b"k0".to_vec(), "best kept");
        assert_eq!(page.last().unwrap().1, b"k4".to_vec(), "worst evicted");

    }

    #[test]
    fn view_catalog_sidecar_roundtrip() {
        let tree = Tree::Diff(
            Box::new(Tree::And(
                Box::new(leaf("age", IndexValue::I64(1), IndexValue::I64(9))),
                Box::new(leaf(
                    "dept",
                    IndexValue::Str(b"e )n%g".to_vec()),
                    IndexValue::Str(b"e )n%g".to_vec()),
                )),
            )),
            Box::new(leaf("flag", IndexValue::F64(-0.5), IndexValue::F64(2.5))),
        );
        let spec = ViewSpec {
            name: b"v one".to_vec(),
            tree,
            order_by: b"age".to_vec(),
            desc: true,
            mode: ViewMode::Materialized { top_k: 50 },
            via: Some(b"user:{key.1}".to_vec()),
        };
        let mut c = ViewCatalog::new();
        c.create(spec.clone()).unwrap();
        c.create(ViewSpec {
            name: b"v2".to_vec(),
            tree: leaf("age", IndexValue::I64(0), IndexValue::I64(1)),
            order_by: b"age".to_vec(),
            desc: false,
            mode: ViewMode::Virtual,
            via: None,
        })
        .unwrap();
        let text = c.to_sidecar();
        let c2 = ViewCatalog::from_sidecar(&text).expect("parse");
        assert_eq!(c2.len(), 2);
        assert_eq!(c2.get(b"v one").unwrap(), &spec);
        assert!(ViewCatalog::from_sidecar("junk").is_none());
    }

    #[test]
    fn materialized_underflow_signals() {
        let mut m = MaterializedSet::new(4);
        for i in 0..5 {
            m.apply(format!("k{i}").as_bytes(), true, Some(IndexValue::I64(i)));
        }
        assert_eq!(m.len(), 5);
        assert!(!m.apply(b"k0", false, None), "5→4 = still K");
        assert!(m.apply(b"k1", false, None), "4→3 < K → underflow signal");
        // order-index-excluded members are counted, not stored
        m.apply(b"kx", true, None);
        assert_eq!(m.order_excluded, 1);
        // unbounded never underflows
        let mut u = MaterializedSet::new(0);
        u.apply(b"a", true, Some(IndexValue::I64(1)));
        assert!(!u.apply(b"a", false, None));
    }
}

/// The view registry (mirrors [`crate::Catalog`]): named specs +
/// sidecar text round-trip. Cap 64.
#[derive(Debug, Clone, Default)]
pub struct ViewCatalog {
    specs: Vec<ViewSpec>,
}

/// Hard cap on declared views.
pub const MAX_VIEWS: usize = 64;

impl ViewCatalog {
    /// Empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register; errors on duplicate/cap/structure.
    pub fn create(&mut self, spec: ViewSpec) -> Result<(), &'static str> {
        spec.validate()?;
        if self.specs.len() >= MAX_VIEWS {
            return Err("ERR view limit reached (64)");
        }
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err("ERR view already exists");
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Drop by name.
    pub fn drop_view(&mut self, name: &[u8]) -> bool {
        let n = self.specs.len();
        self.specs.retain(|s| s.name != name);
        self.specs.len() != n
    }

    /// Lookup.
    pub fn get(&self, name: &[u8]) -> Option<&ViewSpec> {
        self.specs.iter().find(|s| s.name == name)
    }

    /// Declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &ViewSpec> {
        self.specs.iter()
    }

    /// Count.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Sidecar text (one line per view).
    pub fn to_sidecar(&self) -> String {
        let mut out = String::from("kevy-view-catalog v1\n");
        for s in &self.specs {
            out.push_str(&s.to_line());
            out.push('\n');
        }
        out
    }

    /// Parse the sidecar text.
    pub fn from_sidecar(text: &str) -> Option<ViewCatalog> {
        let mut lines = text.lines();
        if lines.next()? != "kevy-view-catalog v1" {
            return None;
        }
        let mut c = ViewCatalog::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            c.create(ViewSpec::from_line(line)?).ok()?;
        }
        Some(c)
    }
}

fn esc(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'%' || c == b'(' || c == b')' || !(33..127).contains(&c) {
            out.push_str(&format!("%{c:02X}"));
        } else {
            out.push(c as char);
        }
    }
    if out.is_empty() { "%".into() } else { out }
}

fn unesc(s: &str) -> Option<Vec<u8>> {
    if s == "%" {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

fn val_ser(v: &IndexValue) -> String {
    match v {
        IndexValue::I64(i) => format!("i{i}"),
        IndexValue::F64(f) => format!("f{}", f.to_bits()),
        IndexValue::Str(s) => format!("s{}", esc(s)),
    }
}

fn val_de(s: &str) -> Option<IndexValue> {
    let (tag, rest) = s.split_at(1);
    match tag {
        "i" => rest.parse().ok().map(IndexValue::I64),
        "f" => rest.parse::<u64>().ok().map(|b| IndexValue::F64(f64::from_bits(b))),
        "s" => unesc(rest).map(IndexValue::Str),
        _ => None,
    }
}

fn tree_ser(t: &Tree, out: &mut String) {
    match t {
        Tree::Leaf(l) => {
            out.push_str(&format!("(L {} {} {})", esc(&l.index), val_ser(&l.min), val_ser(&l.max)));
        }
        Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => {
            let op = match t {
                Tree::And(..) => "A",
                Tree::Or(..) => "O",
                _ => "D",
            };
            out.push_str(&format!("({op} "));
            tree_ser(a, out);
            out.push(' ');
            tree_ser(b, out);
            out.push(')');
        }
    }
}

fn tree_de(toks: &[&str], pos: &mut usize) -> Option<Tree> {
    let t = toks.get(*pos)?;
    *pos += 1;
    match *t {
        "(L" => {
            let idx = unesc(toks.get(*pos)?)?;
            let min = val_de(toks.get(*pos + 1)?)?;
            let max = val_de(toks.get(*pos + 2)?.trim_end_matches(')'))?;
            *pos += 3;
            Some(Tree::Leaf(Leaf { index: idx, min, max }))
        }
        "(A" | "(O" | "(D" => {
            let a = tree_de(toks, pos)?;
            let b = tree_de(toks, pos)?;
            let tree = match *t {
                "(A" => Tree::And(Box::new(a), Box::new(b)),
                "(O" => Tree::Or(Box::new(a), Box::new(b)),
                _ => Tree::Diff(Box::new(a), Box::new(b)),
            };
            Some(tree)
        }
        _ => None,
    }
}

impl ViewSpec {
    /// One sidecar line: `name order_by desc mode topk via tree…`.
    pub fn to_line(&self) -> String {
        let (mode, k) = match self.mode {
            ViewMode::Virtual => ("v", 0),
            ViewMode::Materialized { top_k } => ("m", top_k),
        };
        let via = self.via.as_deref().map(esc).unwrap_or_else(|| "-".into());
        let mut tree = String::new();
        tree_ser(&self.tree, &mut tree);
        format!(
            "{} {} {} {} {} {} {}",
            esc(&self.name),
            esc(&self.order_by),
            u8::from(self.desc),
            mode,
            k,
            via,
            tree
        )
    }

    /// Parse [`Self::to_line`].
    pub fn from_line(line: &str) -> Option<ViewSpec> {
        let toks: Vec<&str> = line.split(' ').collect();
        if toks.len() < 7 {
            return None;
        }
        let mode = match toks[3] {
            "v" => ViewMode::Virtual,
            "m" => ViewMode::Materialized { top_k: toks[4].parse().ok()? },
            _ => return None,
        };
        let via = if toks[5] == "-" { None } else { Some(unesc(toks[5])?) };
        let mut pos = 6;
        // Re-tokenize the tree tail with ')' handling: split keeps
        // parens attached; tree_de trims them.
        let tree = tree_de_root(&toks, &mut pos)?;
        Some(ViewSpec {
            name: unesc(toks[0])?,
            order_by: unesc(toks[1])?,
            desc: toks[2] == "1",
            mode,
            via,
            tree,
        })
    }
}

fn tree_de_root(toks: &[&str], pos: &mut usize) -> Option<Tree> {
    // Fixed arity makes parens redundant on the way back in: each op
    // token consumes exactly two subtrees, each leaf exactly three
    // value tokens (the last with its trailing parens trimmed). The
    // serializer is the only producer; malformed input answers None.
    tree_de(toks, pos)
}
