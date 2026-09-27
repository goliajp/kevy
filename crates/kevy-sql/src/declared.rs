//! A `TABLE.DECLARE` argv read back into the compiler's own [`Table`] —
//! the inverse of [`crate::schema::declare_argv`]. The input is what a
//! server's `TABLE.DESCRIBE` hands out as `declaration`, so a client can
//! plan against the live catalog without a schema file. The parts SQL has
//! no words for (a prefix other than `<table>:`, `WINDOW`, `AUTODECLARE`)
//! are kept beside the table, verbatim, for whoever renders it.

use crate::KevyType;
use crate::schema::{Ix, OrderPath, Table};

/// One declaration: the table plus the clauses outside the SQL subset.
pub(crate) struct Declared {
    pub(crate) table: Table,
    pub(crate) prefix: String,
    /// `WINDOW …` / `AUTODECLARE …` clauses, as their argv words.
    pub(crate) beyond_sql: Vec<Vec<String>>,
}

fn kw(word: Option<&String>, want: &str) -> bool {
    word.is_some_and(|w| w.eq_ignore_ascii_case(want))
}

fn is_clause(word: &str) -> bool {
    ["COLUMN", "INDEX", "ORDERPATH", "WINDOW", "AUTODECLARE"]
        .iter()
        .any(|k| word.eq_ignore_ascii_case(k))
}

/// Read one `TABLE.DECLARE` argv; `Err` names what did not fit.
pub(crate) fn read(argv: &[String]) -> Result<Declared, String> {
    let head_ok = argv.len() >= 9
        && kw(argv.first(), "TABLE.DECLARE")
        && kw(argv.get(2), "PREFIX")
        && kw(argv.get(4), "PK");
    if !head_ok {
        return Err(format!("not a TABLE.DECLARE declaration: {}", argv.join(" ")));
    }
    let mut d = Declared {
        table: Table {
            name: argv[1].clone(),
            pk: argv[5].clone(),
            columns: Vec::new(),
            indexes: Vec::new(),
            orderpaths: Vec::new(),
        },
        prefix: argv[3].clone(),
        beyond_sql: Vec::new(),
    };
    let mut i = 6;
    while i < argv.len() {
        i = clause(argv, i, &mut d)?;
    }
    Ok(d)
}

fn clause(argv: &[String], i: usize, d: &mut Declared) -> Result<usize, String> {
    let word = &argv[i];
    let arg = |n: usize| argv.get(i + n).cloned().ok_or_else(|| format!("{word} is cut short"));
    if word.eq_ignore_ascii_case("COLUMN") {
        let ty = match arg(2)?.to_ascii_lowercase().as_str() {
            "i64" => KevyType::I64,
            "f64" => KevyType::F64,
            "str" => KevyType::Str,
            other => return Err(format!("column type '{other}' is not i64|f64|str")),
        };
        d.table.columns.push((arg(1)?, ty));
        Ok(i + 3)
    } else if word.eq_ignore_ascii_case("INDEX") {
        let unique = arg(2)?.eq_ignore_ascii_case("unique");
        let mut ix = Ix { column: arg(1)?, unique, values: Vec::new() };
        let mut at = i + 3;
        if kw(argv.get(at), "VALUES") {
            at += 1;
            while at < argv.len() && !is_clause(&argv[at]) {
                ix.values.push(argv[at].clone());
                at += 1;
            }
        }
        d.table.indexes.push(ix);
        Ok(at)
    } else if word.eq_ignore_ascii_case("ORDERPATH") {
        orderpath(argv, i, d)
    } else {
        if !is_clause(word) {
            return Err(format!("unknown clause '{word}'"));
        }
        let width = if word.eq_ignore_ascii_case("WINDOW") { 6 } else { 2 };
        let words = argv.get(i..i + width).ok_or_else(|| format!("{word} is cut short"))?;
        d.beyond_sql.push(words.to_vec());
        Ok(i + width)
    }
}

/// `ORDERPATH <name> ON <col> [DESC] [THEN <col> [DESC]]…`.
fn orderpath(argv: &[String], i: usize, d: &mut Declared) -> Result<usize, String> {
    let (Some(name), true) = (argv.get(i + 1), kw(argv.get(i + 2), "ON")) else {
        return Err("ORDERPATH needs <name> ON <col>".into());
    };
    let mut on = Vec::new();
    let mut at = i + 3;
    while let Some(col) = argv.get(at).filter(|w| !is_clause(w)) {
        let desc = kw(argv.get(at + 1), "DESC");
        on.push((col.clone(), desc));
        at += 1 + usize::from(desc);
        if !kw(argv.get(at), "THEN") {
            break;
        }
        at += 1;
    }
    d.table.orderpaths.push(OrderPath { name: name.clone(), on });
    Ok(at)
}
