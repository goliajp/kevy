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
use crate::table::{TableSpec, compile_table};
use crate::{Partitioning, split_point_text};

/// One node of a describe reply. Numbers travel as bulk strings and an
/// absent part as `-`, the same conventions `TABLE.LIST` uses, so a
/// reader of one reads the other.
///
/// ```
/// use kevy_index::{Described, describe_table, parse_table_declare};
///
/// let t = parse_table_declare(&[
///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
/// ])
/// .unwrap();
/// let Described::Array(fields) = describe_table(&t) else { unreachable!() };
/// assert_eq!(fields[0], Described::Bulk(b"name".to_vec()));
/// assert_eq!(fields[13], Described::Bulk(b"-".to_vec())); // no window
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Described {
    /// A bulk string — a name, a keyword, or a number in decimal.
    ///
    /// ```
    /// # use kevy_index::Described;
    /// let maxmem = Described::Bulk(b"1048576".to_vec());
    /// assert!(matches!(maxmem, Described::Bulk(ref n) if n == b"1048576"));
    /// ```
    Bulk(Vec<u8>),
    /// A nested array — a list, or a label/value group.
    ///
    /// ```
    /// # use kevy_index::Described;
    /// let column = Described::Array(vec![
    ///     Described::Bulk(b"age".to_vec()),
    ///     Described::Bulk(b"i64".to_vec()),
    /// ]);
    /// assert!(matches!(column, Described::Array(ref pair) if pair.len() == 2));
    /// ```
    Array(Vec<Described>),
}

pub(crate) fn b(v: impl AsRef<[u8]>) -> Described {
    Described::Bulk(v.as_ref().to_vec())
}

pub(crate) fn n(v: impl ToString) -> Described {
    Described::Bulk(v.to_string().into_bytes())
}

pub(crate) fn flag(v: bool) -> Described {
    n(u8::from(v))
}

pub(crate) fn argv(words: Vec<Vec<u8>>) -> Described {
    Described::Array(words.into_iter().map(Described::Bulk).collect())
}

/// `IDX.DESCRIBE`: `name prefix kind type fields values positions maxmem
/// groupby ann composite table declaration`. An index a table compiled
/// names that table and has no declaration of its own (`-`): it is
/// recreated by its table, and a composite has no `IDX.CREATE` spelling.
///
/// ```
/// use kevy_index::{Described, IndexKind, IndexSpec, ValType, describe_index};
///
/// let s = IndexSpec::single_field(
///     b"age".to_vec(), b"user:".to_vec(), b"age".to_vec(), ValType::I64, IndexKind::Range,
/// );
/// let Described::Array(fields) = describe_index(&s, []) else { unreachable!() };
/// assert_eq!(fields[22], Described::Bulk(b"table".to_vec()));
/// assert_eq!(fields[23], Described::Bulk(b"-".to_vec()));
/// ```
pub fn describe_index<'a>(
    s: &IndexSpec,
    tables: impl IntoIterator<Item = &'a TableSpec>,
) -> Described {
    describe_index_partitioned(s, &Partitioning::Local, tables)
}

/// [`describe_index`] for an index spread as `part` says: a
/// `partitioning` pair before the declaration names it (`local`, or
/// `global` with the split values), and the declaration carries the
/// `PARTITION` options.
///
/// ```
/// use kevy_index::{Described, IndexKind, IndexSpec, Partitioning, ValType, describe_index_partitioned, order_key};
///
/// let s = IndexSpec::single_field(
///     b"age".to_vec(), b"user:".to_vec(), b"age".to_vec(), ValType::I64, IndexKind::Range,
/// );
/// let p = Partitioning::Global { splits: vec![order_key(ValType::I64, b"30").unwrap()] };
/// let Described::Array(fields) = describe_index_partitioned(&s, &p, []) else { unreachable!() };
/// assert_eq!(fields[24], Described::Bulk(b"partitioning".to_vec()));
/// ```
pub fn describe_index_partitioned<'a>(
    s: &IndexSpec,
    part: &Partitioning,
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
        b("partitioning"),
        partitioning_described(s, part),
        b("declaration"),
        declaration_described(owner.is_some(), s, part),
    ])
}

/// The `IDX.CREATE` that recreates the index — `-` for one a table
/// compiled (its table recreates it) or a composite (no such spelling).
fn declaration_described(from_table: bool, s: &IndexSpec, part: &Partitioning) -> Described {
    if from_table || s.composite.is_some() {
        b("-")
    } else {
        argv(index_declaration_partitioned(s, part))
    }
}

/// `local`, or `global` with its split values.
fn partitioning_described(s: &IndexSpec, part: &Partitioning) -> Described {
    match part {
        Partitioning::Local => b("local"),
        Partitioning::Global { splits } => {
            let splits = splits.iter().map(|p| b(split_point_text(s, p))).collect();
            Described::Array(vec![b("global"), Described::Array(splits)])
        }
    }
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
///
/// ```
/// use kevy_index::{owner_of, parse_table_declare};
///
/// let t = parse_table_declare(&[
///     b"TABLE.DECLARE", b"u", b"PREFIX", b"u:", b"PK", b"id",
///     b"COLUMN", b"id", b"i64", b"INDEX", b"id", b"unique",
/// ])
/// .unwrap();
/// let tables = [t];
/// assert_eq!(owner_of(&tables, b"u.id").map(|t| t.name.as_slice()), Some(&b"u"[..]));
/// assert!(owner_of(&tables, b"age").is_none());
/// ```
pub fn owner_of<'a>(
    tables: impl IntoIterator<Item = &'a TableSpec>,
    index: &[u8],
) -> Option<&'a TableSpec> {
    tables
        .into_iter()
        .find(|t| compile_table(t).is_ok_and(|compiled| compiled.iter().any(|c| c.name == index)))
}

/// The `IDX.CREATE` argv that recreates a directly declared index.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, ValType, index_declaration};
///
/// let mut s = IndexSpec::single_field(
///     b"age".to_vec(), b"user:".to_vec(), b"age".to_vec(), ValType::I64, IndexKind::Range,
/// );
/// s.max_bytes = 4096;
/// let line: Vec<String> =
///     index_declaration(&s).iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect();
/// assert_eq!(line.join(" "), "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range MAXMEM 4096");
/// ```
pub fn index_declaration(s: &IndexSpec) -> Vec<Vec<u8>> {
    index_declaration_partitioned(s, &Partitioning::Local)
}

/// [`index_declaration`] with the `PARTITION global` / `SPLIT` options a
/// global index needs to be recreated as it is.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, Partitioning, ValType, index_declaration_partitioned, order_key};
///
/// let s = IndexSpec::single_field(
///     b"age".to_vec(), b"user:".to_vec(), b"age".to_vec(), ValType::I64, IndexKind::Range,
/// );
/// let p = Partitioning::Global { splits: vec![order_key(ValType::I64, b"30").unwrap()] };
/// let line: Vec<String> = index_declaration_partitioned(&s, &p)
///     .iter()
///     .map(|w| String::from_utf8_lossy(w).into_owned())
///     .collect();
/// assert!(line.join(" ").ends_with("KIND range PARTITION global SPLIT 30"));
/// ```
pub fn index_declaration_partitioned(s: &IndexSpec, part: &Partitioning) -> Vec<Vec<u8>> {
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
    if let Partitioning::Global { splits } = part {
        w.extend([b"PARTITION".to_vec(), b"global".to_vec()]);
        for p in splits {
            w.extend([b"SPLIT".to_vec(), split_point_text(s, p)]);
        }
    }
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

pub(crate) fn order(desc: bool) -> &'static str {
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

pub use crate::describe_table::{
    describe_table, describe_table_partitioned, table_declaration, table_declaration_partitioned,
};
pub use crate::describe_view::{describe_view, view_declaration};

#[cfg(test)]
#[path = "describe_tests.rs"]
mod tests;
