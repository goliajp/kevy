//! `TABLE.DESCRIBE` and the declaration that recreates a table. Split
//! from `describe.rs` for the 500-line cap.

use crate::Partitioning;
use crate::catalog::Catalog;
use crate::describe::{Described, argv, b, flag, n, order};
use crate::table::{OrderPath, TableIndex, TableSpec, dotted};

/// `TABLE.DESCRIBE`: `name prefix pk columns indexes orderpaths window
/// autodeclare declaration`, label/value. Indexes and orderpaths carry
/// the compiled path name and whether the auto loop added them.
///
/// ```
/// use kevy_index::{Described, describe_table, parse_table_declare};
///
/// let t = parse_table_declare(&[
///     b"TABLE.DECLARE", b"u", b"PREFIX", b"u:", b"PK", b"id",
///     b"COLUMN", b"id", b"i64", b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range",
/// ])?;
/// let Described::Array(fields) = describe_table(&t) else { unreachable!() };
/// let Described::Array(indexes) = &fields[9] else { unreachable!() };
/// let Described::Array(index) = &indexes[0] else { unreachable!() };
/// assert_eq!(index[1], Described::Bulk(b"u.age".to_vec()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn describe_table(t: &TableSpec) -> Described {
    describe_table_partitioned(t, &Catalog::new())
}

/// [`describe_table`] whose declaration carries each path's `GLOBAL`
/// clause, read from the index catalog `cat` the table compiled into.
///
/// ```
/// use kevy_index::{
///     Catalog, Described, Partitioning, ValType, describe_table_partitioned, order_key,
///     parse_table_declare,
/// };
///
/// let t = parse_table_declare(&[
///     b"TABLE.DECLARE", b"u", b"PREFIX", b"u:", b"PK", b"id", b"COLUMN", b"id", b"i64",
///     b"COLUMN", b"age", b"i64", b"INDEX", b"age", b"range",
/// ])?;
/// let mut cat = Catalog::new();
/// for spec in t.compile()? {
///     let splits = vec![order_key(ValType::I64, b"40").unwrap()];
///     cat.create_with(spec, Partitioning::Global { splits })?;
/// }
/// let Described::Array(fields) = describe_table_partitioned(&t, &cat) else { unreachable!() };
/// let Some(Described::Array(decl)) = fields.last() else { unreachable!() };
/// let tail: Vec<&Described> = decl.iter().rev().take(4).collect();
/// assert_eq!(tail[0], &Described::Bulk(b"40".to_vec()));
/// assert_eq!(tail[3], &Described::Bulk(b"GLOBAL".to_vec()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn describe_table_partitioned(t: &TableSpec, cat: &Catalog) -> Described {
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
        argv(table_declaration_partitioned(&t.sans_auto(), cat)),
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
        let on = op.on.iter().map(|(c, o)| Described::Array(vec![b(c), b(order(*o))]));
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
///
/// ```
/// use kevy_index::{parse_table_declare, table_declaration};
///
/// // Keywords come back upper-case, kinds lower-case: the canonical form.
/// let t = parse_table_declare(&[
///     b"table.declare", b"u", b"prefix", b"u:", b"pk", b"id",
///     b"column", b"id", b"I64", b"index", b"id", b"UNIQUE",
/// ])?;
/// let argv = table_declaration(&t);
/// let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
/// assert_eq!(refs, [&b"TABLE.DECLARE"[..], b"u", b"PREFIX", b"u:", b"PK", b"id",
///     b"COLUMN", b"id", b"i64", b"INDEX", b"id", b"unique"]);
/// assert_eq!(parse_table_declare(&refs)?, t);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn table_declaration(t: &TableSpec) -> Vec<Vec<u8>> {
    table_declaration_partitioned(t, &Catalog::new())
}

/// [`table_declaration`] with each path's `GLOBAL` clause, as the index
/// catalog `cat` holds it, split points included (sampled ones too), so a
/// replay places every value where it was.
///
/// ```
/// use kevy_index::{
///     Catalog, Partitioning, ValType, order_key, parse_table_declare, table_declaration_partitioned,
/// };
///
/// let t = parse_table_declare(&[
///     b"TABLE.DECLARE", b"u", b"PREFIX", b"u:", b"PK", b"id", b"COLUMN", b"id", b"i64",
///     b"INDEX", b"id", b"range",
/// ])?;
/// let mut cat = Catalog::new();
/// for spec in t.compile()? {
///     let splits = vec![order_key(ValType::I64, b"100").unwrap()];
///     cat.create_with(spec, Partitioning::Global { splits })?;
/// }
/// let argv = table_declaration_partitioned(&t, &cat);
/// assert_eq!(argv[argv.len() - 4..], [b"GLOBAL".to_vec(), b"SPLIT".to_vec(), b"AT".to_vec(), b"100".to_vec()]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn table_declaration_partitioned(t: &TableSpec, cat: &Catalog) -> Vec<Vec<u8>> {
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
        index_clause(t, ix, cat, &mut w);
    }
    for op in &t.orderpaths {
        orderpath_clause(t, op, cat, &mut w);
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

/// `INDEX col kind [VALUES …] [GLOBAL [SPLIT AT …]]`.
fn index_clause(t: &TableSpec, ix: &TableIndex, cat: &Catalog, w: &mut Vec<Vec<u8>>) {
    w.extend([b"INDEX".to_vec(), ix.column.clone(), ix.kind.tag().into()]);
    if !ix.values.is_empty() {
        w.push(b"VALUES".to_vec());
        w.extend(ix.values.iter().cloned());
    }
    global_clause(cat, &dotted(&t.name, &ix.column), w);
}

/// `GLOBAL [SPLIT AT …]` for the compiled path `name`, when it is global:
/// every split point in the text [`parse_split_point`] reads back, so a
/// replay places every value where it was.
///
/// [`parse_split_point`]: crate::parse_split_point
fn global_clause(cat: &Catalog, name: &[u8], w: &mut Vec<Vec<u8>>) {
    let (Partitioning::Global { splits }, Some((spec, _))) =
        (cat.partitioning(name), cat.get(name))
    else {
        return;
    };
    w.push(b"GLOBAL".to_vec());
    if !splits.is_empty() {
        w.extend([b"SPLIT".to_vec(), b"AT".to_vec()]);
        w.extend(splits.iter().map(|p| spec.split_point_text(p)));
    }
}

/// `ORDERPATH name ON col [DESC] [THEN col [DESC]]… [GLOBAL [SPLIT AT …]]`.
fn orderpath_clause(t: &TableSpec, op: &OrderPath, cat: &Catalog, w: &mut Vec<Vec<u8>>) {
    w.extend([b"ORDERPATH".to_vec(), op.name.clone(), b"ON".to_vec()]);
    for (i, (c, o)) in op.on.iter().enumerate() {
        if i > 0 {
            w.push(b"THEN".to_vec());
        }
        w.push(c.clone());
        if *o == kevy_text::SortOrder::Desc {
            w.push(b"DESC".to_vec());
        }
    }
    global_clause(cat, &dotted(&t.name, &op.name), w);
}
