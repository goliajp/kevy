//! Pass 2 — compile each `CREATE VIEW` against its table's *declared*
//! access paths. The compiler NEVER plans: the driving predicate either
//! matches a declared index / orderpath (leading-prefix rule) or the
//! compile errors naming the exact missing declaration. Deterministic
//! path choice, documented in the README:
//!
//! 1. constant-only predicates + expressible ORDER BY → an engine
//!    `VIEW.CREATE` (each predicate column individually indexed);
//! 2. else, all predicates on ONE indexed column → a direct-drive card;
//! 3. else, the predicate set matches an ORDERPATH leading prefix → a
//!    composite `WHERE` card;
//! 4. else, the FIRST predicate column (source order) with a usable
//!    index drives, and residuals compile to `FILTER` over the index's
//!    stored `INCLUDE` columns — or the error names what to add.

use crate::ast::CreateView;
use crate::schema::{OrderPath, Table};
use crate::{CardParam, KevyType, QueryCard, SqlError};

use crate::viewplan_norm::{BV, ColPred, Shape, normalize};
use crate::viewplan_view::{try_engine_view, view_read_note};

/// One compiled view: an engine view command or a query card.
pub(crate) enum Planned {
    View(Vec<String>),
    Card(QueryCard),
}

pub(crate) fn plan_view(
    v: &CreateView,
    t: &Table,
    notes: &mut Vec<String>,
) -> Result<Planned, SqlError> {
    let fields = resolve_fields(v, t)?;
    check_counts(v)?;
    let order = resolve_order(v, t)?;
    if v.preds.is_empty() {
        return Err(err_v(
            v,
            format!(
                "a view with no WHERE would scan the table \u{2014} kevy has no scans; add a driving predicate, or page an index directly (IDX.QUERY {}.<col> RANGE \u{2026})",
                t.name
            ),
        ));
    }
    let preds = normalize(v, t)?;
    let constant = !preds.iter().any(ColPred::has_param);
    if constant
        && v.offset.is_none()
        && let Some(argv) = try_engine_view(v, t, &preds, order.as_ref())
    {
        notes.push(view_read_note(v, &fields));
        return Ok(Planned::View(argv));
    }
    let mut fail: Option<String> = None;
    if let Some(card) = card_direct(v, t, &preds, order.as_ref(), &fields, &mut fail)? {
        return Ok(Planned::Card(card));
    }
    if let Some(card) = card_orderpath(v, t, &preds, order.as_ref(), &fields)? {
        return Ok(Planned::Card(card));
    }
    if let Some(card) = card_residual(v, t, &preds, order.as_ref(), &fields, &mut fail)? {
        return Ok(Planned::Card(card));
    }
    Err(no_path_error(v, t, &preds, order.as_ref(), fail))
}

fn err_v(v: &CreateView, msg: impl std::fmt::Display) -> SqlError {
    SqlError::at(v.line, v.col, format!("view '{}': {msg}", v.name))
}

/// `SELECT *` → every declared column; a list validates against the
/// declaration.
fn resolve_fields(v: &CreateView, t: &Table) -> Result<Vec<String>, SqlError> {
    match &v.select {
        None => Ok(t.columns.iter().map(|(n, _)| n.clone()).collect()),
        Some(cols) => {
            for c in cols {
                if t.column_type(c).is_none() {
                    return Err(err_v(
                        v,
                        format!("SELECT names unknown column '{c}' of table '{}'", t.name),
                    ));
                }
            }
            Ok(cols.clone())
        }
    }
}

/// Engine caps, named here instead of silently clamped there.
fn check_counts(v: &CreateView) -> Result<(), SqlError> {
    if let Some(n) = v.limit {
        if n == 0 {
            return Err(err_v(v, "LIMIT 0 selects nothing"));
        }
        if n > 10_000 {
            return Err(err_v(
                v,
                "the engine caps LIMIT at 10000 \u{2014} page with CURSOR (docs/indexes.md)",
            ));
        }
    }
    if let Some(m) = v.offset
        && m > 10_000
    {
        return Err(err_v(
            v,
            "the engine caps OFFSET at 10000 \u{2014} page with CURSOR instead (docs/indexes.md)",
        ));
    }
    Ok(())
}

fn resolve_order(v: &CreateView, t: &Table) -> Result<Option<(String, bool)>, SqlError> {
    match &v.order {
        None => Ok(None),
        Some((c, d)) => {
            if t.column_type(c).is_none() {
                return Err(err_v(
                    v,
                    format!("ORDER BY names unknown column '{c}' of table '{}'", t.name),
                ));
            }
            Ok(Some((c.clone(), *d)))
        }
    }
}

// ───────────── query cards ─────────────

/// Collects argv + `$N` params; `bind` refuses one `$N` spanning two
/// column types (the runtime value could not satisfy both).
struct CardB {
    argv: Vec<String>,
    params: Vec<CardParam>,
}

impl CardB {
    fn new(verb: &str, path: String) -> CardB {
        CardB { argv: vec!["IDX.QUERY".into(), path], params: Vec::new() }.tap_verb(verb)
    }

    fn tap_verb(mut self, verb: &str) -> CardB {
        if !verb.is_empty() {
            self.argv.push(verb.into());
        }
        self
    }

    fn bind(&mut self, v: &CreateView, bv: &BV, col: &str, ty: KevyType) -> Result<(), SqlError> {
        match bv {
            BV::Lit(x) => self.argv.push(x.clone()),
            BV::Param(n) => {
                self.argv.push(format!("${n}"));
                if let Some(prev) = self.params.iter().find(|p| p.n == *n) {
                    if prev.ty != ty {
                        return Err(err_v(
                            v,
                            format!(
                                "${n} binds both '{}' ({}) and '{col}' ({}) \u{2014} use distinct parameters",
                                prev.column,
                                prev.ty.tag(),
                                ty.tag()
                            ),
                        ));
                    }
                } else {
                    self.params.push(CardParam { n: *n, column: col.into(), ty });
                }
            }
        }
        Ok(())
    }

    fn finish(mut self, v: &CreateView, fields: &[String]) -> QueryCard {
        if let Some(m) = v.offset {
            self.argv.push("OFFSET".into());
            self.argv.push(m.to_string());
        }
        if let Some(n) = v.limit {
            self.argv.push("LIMIT".into());
            self.argv.push(n.to_string());
        }
        self.argv.push("FIELDS".into());
        self.argv.extend(fields.iter().cloned());
        self.params.sort_by_key(|p| p.n);
        QueryCard { name: v.name.clone(), argv: self.argv, params: self.params }
    }
}

/// Card path 1 — every predicate on ONE column that carries a usable
/// single-column index.
fn card_direct(
    v: &CreateView,
    t: &Table,
    preds: &[ColPred],
    order: Option<&(String, bool)>,
    fields: &[String],
    fail: &mut Option<String>,
) -> Result<Option<QueryCard>, SqlError> {
    let [p] = preds else { return Ok(None) };
    let Some(ix) = t.index_on(&p.col) else { return Ok(None) };
    if matches!(p.shape, Shape::Range(..)) && ix.unique {
        return Ok(None);
    }
    let mut b = CardB::new("", format!("{}.{}", t.name, p.col));
    push_drive(&mut b, v, p)?;
    if !push_sort(&mut b, v, t, &ix.values, &p.col, order, fail)? {
        return Ok(None);
    }
    Ok(Some(b.finish(v, fields)))
}

/// The driving `EQ v` / `RANGE lo hi` tokens.
fn push_drive(b: &mut CardB, v: &CreateView, p: &ColPred) -> Result<(), SqlError> {
    match &p.shape {
        Shape::Eq(x) => {
            b.argv.push("EQ".into());
            b.bind(v, x, &p.col, p.ty)
        }
        Shape::Range(lo, hi) => {
            b.argv.push("RANGE".into());
            b.bind(v, lo, &p.col, p.ty)?;
            b.bind(v, hi, &p.col, p.ty)
        }
    }
}

/// ORDER BY on a direct-drive card: the driving column ascending is the
/// natural order; anything else needs the column among the index's
/// stored `INCLUDE` values (`SORT` reads stored values). `Ok(false)` =
/// not satisfiable here (the reason lands in `fail`).
fn push_sort(
    b: &mut CardB,
    v: &CreateView,
    t: &Table,
    values: &[String],
    drive_col: &str,
    order: Option<&(String, bool)>,
    fail: &mut Option<String>,
) -> Result<bool, SqlError> {
    let Some((c, d)) = order else { return Ok(true) };
    if c == drive_col && !*d {
        return Ok(true); // the index's own ascending order.
    }
    if values.iter().any(|x| x == c) {
        b.argv.push("SORT".into());
        b.argv.push(c.clone());
        b.argv.push(if *d { "DESC" } else { "ASC" }.into());
        return Ok(true);
    }
    fail.get_or_insert(format!(
        "view '{}': ORDER BY {c}{} needs the column stored on the driving index \u{2014} add INCLUDE ({c}) to CREATE INDEX ON {} ({drive_col}), or declare CREATE INDEX ON {} ({drive_col}, {c}{})",
        v.name,
        if *d { " DESC" } else { "" },
        t.name,
        t.name,
        if *d { " DESC" } else { "" },
    ));
    Ok(false)
}

/// Card path 2 — the predicate set is a leading prefix of a declared
/// ORDERPATH (eq columns pin the prefix, at most one range on the next
/// component), and ORDER BY (if any) is the path's next component.
fn card_orderpath(
    v: &CreateView,
    t: &Table,
    preds: &[ColPred],
    order: Option<&(String, bool)>,
    fields: &[String],
) -> Result<Option<QueryCard>, SqlError> {
    let eqs: Vec<&ColPred> = preds.iter().filter(|p| matches!(p.shape, Shape::Eq(_))).collect();
    let ranges: Vec<&ColPred> =
        preds.iter().filter(|p| matches!(p.shape, Shape::Range(..))).collect();
    if ranges.len() > 1 {
        return Ok(None);
    }
    let Some(op) =
        t.orderpaths.iter().find(|op| orderpath_matches(op, &eqs, ranges.first().copied(), order))
    else {
        return Ok(None);
    };
    let mut b = CardB::new("WHERE", format!("{}.{}", t.name, op.name));
    for (col, _) in &op.on[..eqs.len()] {
        let p = eqs.iter().find(|p| &p.col == col).expect("matched prefix");
        let Shape::Eq(x) = &p.shape else { unreachable!("eqs are Eq") };
        b.argv.push(p.col.clone());
        b.argv.push("EQ".into());
        b.bind(v, x, &p.col, p.ty)?;
    }
    if let Some(p) = ranges.first() {
        let Shape::Range(lo, hi) = &p.shape else { unreachable!("ranges are Range") };
        b.argv.push("RANGE".into());
        b.argv.push(p.col.clone());
        b.bind(v, lo, &p.col, p.ty)?;
        b.bind(v, hi, &p.col, p.ty)?;
    }
    Ok(Some(b.finish(v, fields)))
}

/// The leading-prefix rule, compiler-side: eq columns = the path's
/// first k components (any source order), the range (if any) = the
/// next, and ORDER BY = an eq-pinned column (trivial) or the first
/// unpinned component with the matching direction.
fn orderpath_matches(
    op: &OrderPath,
    eqs: &[&ColPred],
    range: Option<&ColPred>,
    order: Option<&(String, bool)>,
) -> bool {
    let k = eqs.len();
    if op.on.len() < k {
        return false;
    }
    if !op.on[..k].iter().all(|(c, _)| eqs.iter().any(|p| &p.col == c)) {
        return false;
    }
    if let Some(r) = range {
        match op.on.get(k) {
            Some((c, _)) if *c == r.col => {}
            _ => return false,
        }
    }
    match order {
        None => true,
        Some((c, _)) if eqs.iter().any(|p| &p.col == c) => true,
        Some((c, d)) => match op.on.get(k) {
            Some((oc, od)) => oc == c && od == d,
            None => false,
        },
    }
}

/// Card path 3 — the FIRST predicate (source order) with a usable index
/// drives; residual predicates read the index's stored `INCLUDE`
/// columns as `FILTER` clauses.
fn card_residual(
    v: &CreateView,
    t: &Table,
    preds: &[ColPred],
    order: Option<&(String, bool)>,
    fields: &[String],
    fail: &mut Option<String>,
) -> Result<Option<QueryCard>, SqlError> {
    let Some(di) = preds.iter().position(|p| {
        t.index_on(&p.col).is_some_and(|ix| !(matches!(p.shape, Shape::Range(..)) && ix.unique))
    }) else {
        return Ok(None);
    };
    let drive = &preds[di];
    let ix = t.index_on(&drive.col).expect("position found it");
    let mut b = CardB::new("", format!("{}.{}", t.name, drive.col));
    push_drive(&mut b, v, drive)?;
    for (i, p) in preds.iter().enumerate() {
        if i == di {
            continue;
        }
        if !ix.values.iter().any(|x| x == &p.col) {
            fail.get_or_insert(format!(
                "view '{}': residual WHERE on '{}' reads a stored column that index {}.{} does not carry \u{2014} add INCLUDE ({}) to CREATE INDEX ON {} ({})",
                v.name, p.col, t.name, drive.col, p.col, t.name, drive.col,
            ));
            return Ok(None);
        }
        b.argv.push("FILTER".into());
        b.argv.push(p.col.clone());
        match &p.shape {
            Shape::Eq(x) => {
                b.argv.push("EQ".into());
                b.bind(v, x, &p.col, p.ty)?;
            }
            Shape::Range(lo, hi) => {
                b.argv.push("RANGE".into());
                b.bind(v, lo, &p.col, p.ty)?;
                b.bind(v, hi, &p.col, p.ty)?;
            }
        }
    }
    if !push_sort(&mut b, v, t, &ix.values, &drive.col, order, fail)? {
        return Ok(None);
    }
    Ok(Some(b.finish(v, fields)))
}

/// No declared path serves the view: name the exact declaration to add
/// (the accumulated specific reason wins when one exists).
fn no_path_error(
    v: &CreateView,
    t: &Table,
    preds: &[ColPred],
    order: Option<&(String, bool)>,
    fail: Option<String>,
) -> SqlError {
    if let Some(f) = fail {
        return SqlError::at(v.line, v.col, f);
    }
    let desc: Vec<String> = preds
        .iter()
        .map(|p| {
            format!("{} {}", p.col, if matches!(p.shape, Shape::Eq(_)) { "EQ" } else { "range" })
        })
        .collect();
    let mut cols: Vec<(String, bool)> = Vec::new();
    for p in preds.iter().filter(|p| matches!(p.shape, Shape::Eq(_))) {
        cols.push((p.col.clone(), false));
    }
    for p in preds.iter().filter(|p| matches!(p.shape, Shape::Range(..))) {
        cols.push((p.col.clone(), false));
    }
    if let Some((c, d)) = order
        && !cols.iter().any(|(n, _)| n == c)
    {
        cols.push((c.clone(), *d));
    } else if let Some((c, d)) = order
        && *d
        && let Some(last) = cols.iter_mut().find(|(n, _)| n == c)
    {
        last.1 = true;
    }
    let sugg: Vec<String> =
        cols.iter().map(|(c, d)| if *d { format!("{c} DESC") } else { c.clone() }).collect();
    err_v(
        v,
        format!(
            "WHERE ({}) matches no declared access path \u{2014} add: CREATE INDEX ON {} ({})",
            desc.join(", "),
            t.name,
            sugg.join(", "),
        ),
    )
}
