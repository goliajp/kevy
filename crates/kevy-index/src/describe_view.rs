//! `VIEW.DESCRIBE` — a view's declaration read back (split from
//! `describe.rs` for the 500-line rule; the reply conventions are there).

use crate::describe::{Described, argv, b, flag, n};
use crate::value::IndexValue;
use crate::view::{Tree, ViewMode, ViewSpec};

/// `VIEW.DESCRIBE`: `name query order_by desc mode topk via
/// declaration`. `query` is the tree as the tokens `VIEW.CREATE` reads.
///
/// ```
/// use kevy_index::{Described, IndexValue, Leaf, Tree, ViewMode, ViewSpec, describe_view};
///
/// let v = ViewSpec {
///     name: b"adults".to_vec(),
///     tree: Tree::Leaf(Leaf { index: b"age".to_vec(), min: IndexValue::I64(18), max: IndexValue::I64(99) }),
///     order_by: b"age".to_vec(),
///     desc: false,
///     mode: ViewMode::Virtual,
///     via: None,
/// };
/// let Described::Array(fields) = describe_view(&v) else { unreachable!() };
/// let query = Described::Array(
///     ["age", "RANGE", "18", "99"].iter().map(|w| Described::Bulk(w.as_bytes().to_vec())).collect(),
/// );
/// assert_eq!(fields[3], query);
/// ```
pub fn describe_view(v: &ViewSpec) -> Described {
    let (mode, top_k) = match v.mode {
        ViewMode::Virtual => ("virtual", 0),
        ViewMode::Materialized { top_k } => ("materialized", top_k),
    };
    let mut query = Vec::new();
    tree_words(&v.tree, &mut query);
    Described::Array(vec![
        b("name"),
        b(&v.name),
        b("query"),
        argv(query),
        b("order_by"),
        b(&v.order_by),
        b("desc"),
        flag(v.desc),
        b("mode"),
        b(mode),
        b("topk"),
        n(top_k),
        b("via"),
        v.via.as_ref().map_or_else(|| b("-"), b),
        b("declaration"),
        argv(view_declaration(v)),
    ])
}

/// The `VIEW.CREATE` argv that recreates `v`.
///
/// ```
/// use kevy_index::{IndexValue, Leaf, Tree, ViewMode, ViewSpec, view_declaration};
///
/// let leaf = |i: &str, v: i64| {
///     Box::new(Tree::Leaf(Leaf { index: i.into(), min: IndexValue::I64(v), max: IndexValue::I64(v) }))
/// };
/// let v = ViewSpec {
///     name: b"both".to_vec(),
///     tree: Tree::And(leaf("a", 1), leaf("b", 2)),
///     order_by: b"a".to_vec(),
///     desc: true,
///     mode: ViewMode::Materialized { top_k: 0 },
///     via: None,
/// };
/// let line: Vec<String> =
///     view_declaration(&v).iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect();
/// assert_eq!(line.join(" "), "VIEW.CREATE both QUERY ( AND a EQ 1 b EQ 2 ) ORDER BY a DESC MODE materialized");
/// ```
pub fn view_declaration(v: &ViewSpec) -> Vec<Vec<u8>> {
    let mut w: Vec<Vec<u8>> = vec![b"VIEW.CREATE".to_vec(), v.name.clone(), b"QUERY".to_vec()];
    tree_words(&v.tree, &mut w);
    w.extend([b"ORDER".to_vec(), b"BY".to_vec(), v.order_by.clone()]);
    if v.desc {
        w.push(b"DESC".to_vec());
    }
    if let ViewMode::Materialized { top_k } = v.mode {
        w.extend([b"MODE".to_vec(), b"materialized".to_vec()]);
        if top_k != 0 {
            w.extend([b"TOPK".to_vec(), top_k.to_string().into()]);
        }
    }
    if let Some(via) = &v.via {
        w.extend([b"VIA".to_vec(), via.clone()]);
    }
    w
}

fn tree_words(t: &Tree, w: &mut Vec<Vec<u8>>) {
    let (op, l, r) = match t {
        Tree::Leaf(leaf) => {
            w.push(leaf.index.clone());
            if leaf.min == leaf.max {
                w.extend([b"EQ".to_vec(), literal(&leaf.min)]);
            } else {
                w.extend([b"RANGE".to_vec(), literal(&leaf.min), literal(&leaf.max)]);
            }
            return;
        }
        Tree::And(l, r) => ("AND", l, r),
        Tree::Or(l, r) => ("OR", l, r),
        Tree::Diff(l, r) => ("DIFF", l, r),
    };
    w.extend([b"(".to_vec(), op.into()]);
    tree_words(l, w);
    tree_words(r, w);
    w.push(b")".to_vec());
}

/// A bound as the literal that coerces back to it. Rust's float
/// `Display` is the shortest text that parses back to the same bits.
fn literal(v: &IndexValue) -> Vec<u8> {
    match v {
        IndexValue::I64(i) => i.to_string().into_bytes(),
        IndexValue::F64(f) => f.to_string().into_bytes(),
        IndexValue::Str(s) => s.clone(),
    }
}
