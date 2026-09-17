//! `TABLE.DESCRIBE` / `IDX.DESCRIBE` / `VIEW.DESCRIBE` — a declaration
//! read back.
//!
//! The reply tree is built here, once, and both wire faces (the server
//! and the embedded dispatch) only encode it — so the two cannot
//! disagree about a field's name, order or spelling. Every reply ends
//! with `declaration`: the argv that recreates the object, which is what
//! `dump --schema` and `show-create` print. It is rendered from the
//! spec, not remembered from the original command, so it is the
//! canonical spelling: keywords upper-case, defaults written out where
//! the grammar needs them, the auto loop's additions left out (they are
//! runtime provenance, not declaration intent — [`TableSpec::sans_auto`]).

use crate::catalog::{IndexKind, IndexSpec, ValType};
use crate::table::{TableSpec, compile_table, dotted};
use crate::value::IndexValue;
use crate::view::{Tree, ViewMode, ViewSpec};

/// One node of a describe reply. Numbers travel as bulk strings and an
/// absent part as `-`, the same conventions `TABLE.LIST` uses, so a
/// reader of one reads the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Described {
    /// A bulk string.
    Bulk(Vec<u8>),
    /// A nested array.
    Array(Vec<Described>),
}

fn b(v: impl AsRef<[u8]>) -> Described {
    Described::Bulk(v.as_ref().to_vec())
}

fn n(v: impl ToString) -> Described {
    Described::Bulk(v.to_string().into_bytes())
}

fn flag(v: bool) -> Described {
    n(u8::from(v))
}

fn argv(words: Vec<Vec<u8>>) -> Described {
    Described::Array(words.into_iter().map(Described::Bulk).collect())
}

/// `TABLE.DESCRIBE`: `name prefix pk columns indexes orderpaths window
/// autodeclare declaration`, label/value. Indexes and orderpaths carry
/// the compiled path name and whether the auto loop added them.
pub fn describe_table(t: &TableSpec) -> Described {
    let columns = t.columns.iter().map(|(c, ty)| Described::Array(vec![b(c), b(ty.tag())]));
    let window = match &t.window {
        None => b("-"),
        Some(w) => Described::Array(vec![
            b("column"),
            b(&w.column),
            b("span"),
            n(w.span),
            b("bucket"),
            n(w.bucket),
        ]),
    };
    Described::Array(vec![
        b("name"),
        b(&t.name),
        b("prefix"),
        b(&t.prefix),
        b("pk"),
        b(&t.pk),
        b("columns"),
        Described::Array(columns.collect()),
        b("indexes"),
        table_indexes(t),
        b("orderpaths"),
        table_orderpaths(t),
        b("window"),
        window,
        b("autodeclare"),
        n(t.autodeclare),
        b("declaration"),
        argv(table_declaration(&t.sans_auto())),
    ])
}

fn table_indexes(t: &TableSpec) -> Described {
    let rows = t.indexes.iter().map(|ix| {
        let path = dotted(&t.name, &ix.column);
        Described::Array(vec![
            b("path"),
            b(&path),
            b("column"),
            b(&ix.column),
            b("kind"),
            b(ix.kind.tag()),
            b("values"),
            argv(ix.values.clone()),
            b("auto"),
            flag(t.auto_added.contains(&path)),
        ])
    });
    Described::Array(rows.collect())
}

fn table_orderpaths(t: &TableSpec) -> Described {
    let rows = t.orderpaths.iter().map(|op| {
        let path = dotted(&t.name, &op.name);
        let on = op.on.iter().map(|(c, desc)| Described::Array(vec![b(c), b(order(*desc))]));
        Described::Array(vec![
            b("path"),
            b(&path),
            b("name"),
            b(&op.name),
            b("on"),
            Described::Array(on.collect()),
            b("auto"),
            flag(t.auto_added.contains(&path)),
        ])
    });
    Described::Array(rows.collect())
}

/// The `TABLE.DECLARE` argv that recreates `t` as written.
pub fn table_declaration(t: &TableSpec) -> Vec<Vec<u8>> {
    let mut w: Vec<Vec<u8>> = vec![
        b"TABLE.DECLARE".to_vec(),
        t.name.clone(),
        b"PREFIX".to_vec(),
        t.prefix.clone(),
        b"PK".to_vec(),
        t.pk.clone(),
    ];
    for (c, ty) in &t.columns {
        w.extend([b"COLUMN".to_vec(), c.clone(), ty.tag().into()]);
    }
    for ix in &t.indexes {
        w.extend([b"INDEX".to_vec(), ix.column.clone(), ix.kind.tag().into()]);
        if !ix.values.is_empty() {
            w.push(b"VALUES".to_vec());
            w.extend(ix.values.iter().cloned());
        }
    }
    for op in &t.orderpaths {
        w.extend([b"ORDERPATH".to_vec(), op.name.clone(), b"ON".to_vec()]);
        for (i, (c, desc)) in op.on.iter().enumerate() {
            if i > 0 {
                w.push(b"THEN".to_vec());
            }
            w.push(c.clone());
            if *desc {
                w.push(b"DESC".to_vec());
            }
        }
    }
    if let Some(win) = &t.window {
        w.extend([b"WINDOW".to_vec(), win.column.clone(), b"SPAN".to_vec()]);
        w.extend([win.span.to_string().into(), b"BUCKET".to_vec(), win.bucket.to_string().into()]);
    }
    if t.autodeclare != 0 {
        w.extend([b"AUTODECLARE".to_vec(), t.autodeclare.to_string().into()]);
    }
    w
}

/// `IDX.DESCRIBE`: `name prefix kind type fields values positions maxmem
/// groupby ann composite table declaration`. An index a table compiled
/// names that table and has no declaration of its own (`-`): it is
/// recreated by its table, and a composite has no `IDX.CREATE` spelling.
pub fn describe_index<'a>(
    s: &IndexSpec,
    tables: impl IntoIterator<Item = &'a TableSpec>,
) -> Described {
    let owner = owner_of(tables, &s.name);
    let fields = s.fields.iter().map(|f| Described::Array(vec![b(&f.name), n(f.weight)]));
    let values = s.values.iter().map(|v| Described::Array(vec![b(&v.name), b(v.ty.tag())]));
    let composite = match &s.composite {
        None => b("-"),
        Some(cols) => Described::Array(
            cols.iter()
                .map(|c| Described::Array(vec![b(&c.name), b(c.ty.tag()), b(order(c.desc))]))
                .collect(),
        ),
    };
    Described::Array(vec![
        b("name"),
        b(&s.name),
        b("prefix"),
        b(&s.prefix),
        b("kind"),
        b(s.kind.tag()),
        b("type"),
        b(s.ty.tag()),
        b("fields"),
        Described::Array(fields.collect()),
        b("values"),
        Described::Array(values.collect()),
        b("positions"),
        flag(s.with_positions),
        b("maxmem"),
        n(s.max_bytes),
        b("groupby"),
        s.group_by.as_ref().map_or_else(|| b("-"), b),
        b("ann"),
        index_ann(s),
        b("composite"),
        composite,
        b("table"),
        owner.map_or_else(|| b("-"), |t| b(&t.name)),
        b("declaration"),
        if owner.is_some() || s.composite.is_some() { b("-") } else { argv(index_declaration(s)) },
    ])
}

fn index_ann(s: &IndexSpec) -> Described {
    let Some(a) = &s.ann else { return b("-") };
    Described::Array(vec![
        b("dim"),
        n(a.dim),
        b("distance"),
        b(distance_tag(a.distance)),
        b("m"),
        n(a.m),
        b("ef"),
        n(a.ef),
    ])
}

/// The table whose compile produced the index named `index`.
pub fn owner_of<'a>(
    tables: impl IntoIterator<Item = &'a TableSpec>,
    index: &[u8],
) -> Option<&'a TableSpec> {
    tables
        .into_iter()
        .find(|t| compile_table(t).is_ok_and(|compiled| compiled.iter().any(|c| c.name == index)))
}

/// The `IDX.CREATE` argv that recreates a directly declared index.
pub fn index_declaration(s: &IndexSpec) -> Vec<Vec<u8>> {
    let mut w: Vec<Vec<u8>> =
        vec![b"IDX.CREATE".to_vec(), s.name.clone(), b"ON".to_vec(), b"PREFIX".to_vec()];
    w.push(s.prefix.clone());
    // `FIELDS` (and `WEIGHTS` after it) is the text-only spelling; one
    // neutrally weighted field is written the way every kind accepts.
    let weighted = s.fields.iter().any(|f| f.weight != 1.0);
    if s.kind == IndexKind::Text && (s.fields.len() > 1 || weighted) {
        w.push(b"FIELDS".to_vec());
        w.extend(s.fields.iter().map(|f| f.name.clone()));
        if weighted {
            w.push(b"WEIGHTS".to_vec());
            w.extend(s.fields.iter().map(|f| f.weight.to_string().into_bytes()));
        }
    } else {
        w.push(b"FIELD".to_vec());
        w.extend(s.fields.iter().map(|f| f.name.clone()));
    }
    w.extend([b"TYPE".to_vec(), s.ty.tag().into(), b"KIND".to_vec(), s.kind.tag().into()]);
    index_options(s, &mut w);
    w
}

/// The optional tail of an `IDX.CREATE`, in the order the usage line
/// lists it.
fn index_options(s: &IndexSpec, w: &mut Vec<Vec<u8>>) {
    if s.with_positions {
        w.extend([b"WITH".to_vec(), b"POSITIONS".to_vec()]);
    }
    if !s.values.is_empty() {
        w.push(b"VALUES".to_vec());
        w.extend(s.values.iter().map(|v| v.name.clone()));
        if s.values.iter().any(|v| v.ty != ValType::Str) {
            w.push(b"TYPES".to_vec());
            w.extend(s.values.iter().map(|v| v.ty.tag().as_bytes().to_vec()));
        }
    }
    if s.max_bytes != 0 {
        w.extend([b"MAXMEM".to_vec(), s.max_bytes.to_string().into()]);
    }
    if let Some(g) = &s.group_by {
        w.extend([b"GROUPBY".to_vec(), g.clone()]);
    }
    if let Some(a) = &s.ann {
        w.extend([b"DIM".to_vec(), a.dim.to_string().into()]);
        w.extend([b"DISTANCE".to_vec(), distance_tag(a.distance).into()]);
        w.extend([b"M".to_vec(), a.m.to_string().into(), b"EF".to_vec(), a.ef.to_string().into()]);
    }
}

/// `VIEW.DESCRIBE`: `name query order_by desc mode topk via
/// declaration`. `query` is the tree as the tokens `VIEW.CREATE` reads.
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

fn order(desc: bool) -> &'static str {
    if desc { "desc" } else { "asc" }
}

/// The sidecar's distance code as `IDX.CREATE` spells it (kevy-vector's
/// `Distance` tags; the server's round-trip test holds the two together).
fn distance_tag(code: u8) -> &'static str {
    match code {
        0 => "cosine",
        1 => "l2",
        2 => "ip",
        // Not a code the parser admits; spelled so a recreate refuses
        // by name instead of silently becoming cosine.
        _ => "unknown",
    }
}

#[cfg(test)]
#[path = "describe_tests.rs"]
mod tests;
