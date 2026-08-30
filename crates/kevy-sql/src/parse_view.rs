//! `CREATE VIEW … AS SELECT` — the single query shape that parses,
//! and the long tail of query-time SQL that errors by name (JOIN,
//! subqueries, OR, GROUP BY, expressions, …). Split from `parse.rs`
//! for the 500-LOC house rule.

use crate::SqlError;
use crate::ast::{Bound, CreateView, Pred, PredOp, Stmt};
use crate::lex::Tok;
use crate::parse::P;

pub(crate) fn parse_create_view(p: &mut P<'_>) -> Result<Stmt, SqlError> {
    let (name, line, col) = p.ident("a view name")?;
    p.expect_kw("as", "after the view name")?;
    p.expect_kw("select", "after AS")?;
    let select = parse_select_list(p)?;
    p.expect_kw("from", "after the select list")?;
    let (table, ..) = p.ident("the table name")?;
    check_from_tail(p)?;
    let mut v = CreateView {
        name,
        select,
        table,
        preds: Vec::new(),
        order: None,
        limit: None,
        offset: None,
        line,
        col,
    };
    if p.eat_kw("where") {
        v.preds = parse_preds(p)?;
    }
    parse_view_tail(p, &mut v)?;
    p.expect_sym(';', "after CREATE VIEW")?;
    Ok(Stmt::View(v))
}

/// `*` or a bare column list; aliases / functions / DISTINCT refuse.
fn parse_select_list(p: &mut P<'_>) -> Result<Option<Vec<String>>, SqlError> {
    if p.is_kw("distinct") {
        return Err(p.refuse(
            "SELECT DISTINCT",
            "the engine's DISTINCT exists as a query-time clause \u{2014} add DISTINCT <col> to the compiled IDX.QUERY card by hand (docs/indexes.md)",
        ));
    }
    if p.eat_sym('*') {
        return Ok(None);
    }
    let mut cols = Vec::new();
    loop {
        let (c, ..) = p.ident("a column in the select list")?;
        if p.is_sym('(') {
            return Err(refuse_function(p, &c));
        }
        if p.is_sym('.') {
            return Err(p.refuse(
                &format!("the qualified column '{c}.\u{2026}'"),
                "single-table views reference their columns unqualified",
            ));
        }
        if p.is_kw("as") {
            return Err(p.refuse(
                "a column alias",
                "the card's FIELDS clause returns fields under their declared names",
            ));
        }
        cols.push(c);
        if p.eat_sym(',') {
            continue;
        }
        return Ok(Some(cols));
    }
}

/// Everything that may follow `FROM <t>` and is not ours: joins,
/// aliases, second tables.
fn check_from_tail(p: &mut P<'_>) -> Result<(), SqlError> {
    if p.is_sym(',') {
        return Err(refuse_join(p, "an implicit join (comma FROM)"));
    }
    if p.is_kw("join")
        || p.is_kw("left")
        || p.is_kw("right")
        || p.is_kw("inner")
        || p.is_kw("outer")
        || p.is_kw("cross")
        || p.is_kw("full")
    {
        return Err(refuse_join(p, "JOIN"));
    }
    // A bare identifier after the table is an alias.
    if matches!(p.peek().tok, Tok::Ident(_) | Tok::QIdent(_)) && !is_view_tail_kw(p) {
        return Err(
            p.refuse("a table alias", "single-table views reference their columns unqualified")
        );
    }
    Ok(())
}

fn is_view_tail_kw(p: &P<'_>) -> bool {
    p.is_kw("where")
        || p.is_kw("order")
        || p.is_kw("limit")
        || p.is_kw("offset")
        || p.is_kw("group")
        || p.is_kw("having")
        || p.is_kw("union")
        || p.is_kw("intersect")
        || p.is_kw("except")
}

fn refuse_join(p: &P<'_>, name: &str) -> SqlError {
    p.refuse(
        name,
        "kevy refuses query-time joins (Law 3); model the lookup with an indexed FK column (IDX.QUERY t.fk EQ \u{2026}) or app-side assembly (cookbook \u{a7}2)",
    )
}

fn refuse_function(p: &P<'_>, name: &str) -> SqlError {
    p.refuse(
        &format!("the function call '{name}(\u{2026})'"),
        "kevy evaluates no expressions at query time (Law 3); compute the value app-side at write time and store it as a column (cookbook \u{a7}21)",
    )
}

/// `ORDER BY` / `LIMIT` / `OFFSET` and the refused tail keywords.
fn parse_view_tail(p: &mut P<'_>, v: &mut CreateView) -> Result<(), SqlError> {
    loop {
        if p.is_kw("group") {
            return Err(p.refuse(
                "GROUP BY",
                "aggregates are maintained at write time (IDX.CREATE KIND agg, cookbook \u{a7}20), never computed per query",
            ));
        }
        if p.is_kw("having") {
            return Err(p.refuse(
                "HAVING",
                "there is no query-time aggregation to filter (Law 3); maintain the aggregate at write time and test it app-side",
            ));
        }
        if p.is_kw("union") || p.is_kw("intersect") || p.is_kw("except") {
            return Err(p.refuse(
                "UNION/INTERSECT/EXCEPT",
                "set composition is the engine's view tree \u{2014} declare VIEW.CREATE \u{2026} QUERY ( OR|AND|DIFF \u{2026} ) by hand (docs/views.md)",
            ));
        }
        if p.eat_kw("order") {
            p.expect_kw("by", "after ORDER")?;
            parse_order_by(p, v)?;
        } else if p.eat_kw("limit") {
            v.limit = Some(parse_count(p, "LIMIT")?);
        } else if p.eat_kw("offset") {
            v.offset = Some(parse_count(p, "OFFSET")?);
        } else {
            return Ok(());
        }
    }
}

fn parse_order_by(p: &mut P<'_>, v: &mut CreateView) -> Result<(), SqlError> {
    if v.order.is_some() {
        return Err(p.err_here("duplicate ORDER BY"));
    }
    let (c, ..) = p.ident("the ORDER BY column")?;
    let desc = if p.eat_kw("desc") {
        true
    } else {
        p.eat_kw("asc");
        false
    };
    if p.is_sym(',') {
        return Err(p.refuse(
            "a multi-column ORDER BY",
            "a composite order is a declared path \u{2014} CREATE INDEX ON <t> (a, b [DESC]) and let the ORDERPATH carry the tuple order",
        ));
    }
    v.order = Some((c, desc));
    Ok(())
}

fn parse_count(p: &mut P<'_>, what: &str) -> Result<u64, SqlError> {
    let t = p.peek();
    match &t.tok {
        Tok::Num(s) if !s.contains('.') => {
            let n = s
                .parse::<u64>()
                .map_err(|_| p.err_here(format!("{what} count '{s}' is out of range")))?;
            p.bump();
            Ok(n)
        }
        Tok::Param(_) => Err(p.refuse(
            &format!("a parameterized {what}"),
            "the card's clause is plain text \u{2014} set the count in the runtime call instead",
        )),
        Tok::Ident(s) if s == "all" => Err(p.refuse("LIMIT ALL", "omit LIMIT instead")),
        _ => Err(p.err_here(format!("expected an integer after {what}"))),
    }
}

// ───────────── WHERE predicates ─────────────

fn parse_preds(p: &mut P<'_>) -> Result<Vec<Pred>, SqlError> {
    let mut preds = Vec::new();
    loop {
        preds.push(parse_pred(p)?);
        if p.eat_kw("and") {
            continue;
        }
        if p.is_kw("or") {
            return Err(p.refuse(
                "OR",
                "subset v1 compiles AND chains only; the engine composes OR as a view tree \u{2014} declare VIEW.CREATE \u{2026} QUERY ( OR <leg> <leg> ) by hand (docs/views.md)",
            ));
        }
        return Ok(preds);
    }
}

fn parse_pred(p: &mut P<'_>) -> Result<Pred, SqlError> {
    if p.is_sym('(') {
        return Err(p.refuse(
            "a parenthesized expression / subquery",
            "kevy evaluates nothing at query time (Law 3); compose named views or run two queries app-side",
        ));
    }
    if p.is_kw("not") {
        return Err(p.refuse(
            "NOT",
            "an index serves points and ranges, not exclusions; test the complement app-side",
        ));
    }
    if p.is_kw("exists") {
        return Err(p.refuse(
            "EXISTS",
            "a subquery is query-time evaluation (Law 3); run the inner lookup as its own query",
        ));
    }
    let (line, col) = (p.peek().line, p.peek().col);
    let (column, ..) = p.ident("a column in WHERE")?;
    if p.is_sym('(') {
        return Err(refuse_function(p, &column));
    }
    if p.is_sym('.') {
        return Err(p.refuse(
            &format!("the qualified column '{column}.\u{2026}'"),
            "single-table views reference their columns unqualified",
        ));
    }
    let (op, a, b) = parse_comparison(p)?;
    Ok(Pred { column, op, a, b, line, col })
}

/// The operator + right-hand side(s) of one predicate.
fn parse_comparison(p: &mut P<'_>) -> Result<(PredOp, Bound, Option<Bound>), SqlError> {
    if p.eat_kw("between") {
        let a = parse_value(p)?;
        p.expect_kw("and", "in BETWEEN")?;
        let b = parse_value(p)?;
        return Ok((PredOp::Between, a, Some(b)));
    }
    for (kw, teach) in REFUSED_PRED_KWS {
        if p.is_kw(kw) {
            return Err(p.refuse(&kw.to_ascii_uppercase(), teach));
        }
    }
    let op = match &p.peek().tok {
        Tok::Op("=") => PredOp::Eq,
        Tok::Op(">") => PredOp::Gt,
        Tok::Op(">=") => PredOp::Ge,
        Tok::Op("<") => PredOp::Lt,
        Tok::Op("<=") => PredOp::Le,
        Tok::Op("<>" | "!=") => {
            return Err(p.refuse(
                "'!=' / '<>'",
                "an index serves points and ranges, not exclusions; test it app-side or query the complement ranges",
            ));
        }
        Tok::Sym('+' | '-' | '*' | '/') => {
            return Err(refuse_arith(p));
        }
        _ => return Err(p.err_here("expected a comparison (= > >= < <= BETWEEN)")),
    };
    p.bump();
    let a = parse_value(p)?;
    Ok((op, a, None))
}

const REFUSED_PRED_KWS: &[(&str, &str)] = &[
    ("in", "subset v1 has no IN \u{2014} run one EQ query per member (points on the same index)"),
    (
        "like",
        "pattern scans are refused; declare a text index for token search (docs/text-search.md)",
    ),
    (
        "ilike",
        "pattern scans are refused; declare a text index for token search (docs/text-search.md)",
    ),
    (
        "is",
        "NULL is an absent field, and absent fields leave the index entirely; model presence as a flag column (cookbook \u{a7}7)",
    ),
];

fn refuse_arith(p: &P<'_>) -> SqlError {
    p.refuse(
        "an arithmetic expression",
        "kevy evaluates no expressions at query time (Law 3); store the derived value as its own column at write time (cookbook \u{a7}21)",
    )
}

/// One bound value: literal, `$N`, or a named refusal.
fn parse_value(p: &mut P<'_>) -> Result<Bound, SqlError> {
    if p.is_sym('(') {
        return Err(p.refuse(
            "a parenthesized expression / subquery",
            "kevy evaluates nothing at query time (Law 3); compose named views or run two queries app-side",
        ));
    }
    let neg = p.eat_sym('-');
    let t = p.peek().clone();
    let bound = match &t.tok {
        Tok::Num(s) => {
            p.bump();
            Bound::Num(if neg { format!("-{s}") } else { s.clone() })
        }
        Tok::Str(s) if !neg => {
            p.bump();
            Bound::Str(s.clone())
        }
        Tok::Param(n) if !neg => {
            p.bump();
            Bound::Param(*n)
        }
        Tok::Ident(s) if !neg && (s == "true" || s == "false") => {
            return Err(p.refuse(
                &format!("the bool literal {s}"),
                "bool columns map to str \u{2014} write '1' / '0'",
            ));
        }
        Tok::Ident(s) | Tok::QIdent(s) if !neg => {
            return Err(p.refuse(
                &format!("the column reference '{s}'"),
                "kevy compiles no column-to-column comparisons; quote it ('\u{2026}') if it is a literal",
            ));
        }
        _ => return Err(p.err_here("expected a literal or $N parameter")),
    };
    if matches!(p.peek().tok, Tok::Sym('+' | '-' | '*' | '/')) {
        return Err(refuse_arith(p));
    }
    Ok(bound)
}
