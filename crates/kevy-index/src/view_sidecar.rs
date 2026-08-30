//! View catalog + sidecar text round-trip — the persistence face of
//! [`crate::view`] (split out to keep `view.rs` under the 500-LOC
//! project ceiling; behaviour unchanged).

use crate::value::IndexValue;
use crate::view::{Leaf, Tree, ViewMode, ViewSpec};
use std::fmt::Write as _;

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
        if c == b' '
            || c == b'\t'
            || c == b'\n'
            || c == b'%'
            || c == b'('
            || c == b')'
            || !(33..127).contains(&c)
        {
            let _ = write!(out, "%{c:02X}");
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
            let _ = write!(out, "(L {} {} {})", esc(&l.index), val_ser(&l.min), val_ser(&l.max));
        }
        Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => {
            let op = match t {
                Tree::And(..) => "A",
                Tree::Or(..) => "O",
                _ => "D",
            };
            let _ = write!(out, "({op} ");
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
        let via = self.via.as_deref().map_or_else(|| "-".into(), esc);
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
