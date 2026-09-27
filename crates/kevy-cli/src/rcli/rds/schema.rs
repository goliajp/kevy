//! `show-create <name> [--as kevy|sql]` and `dump --schema [--table t]…
//! [--as kevy|sql]`: declarations read back from the server, written as
//! commands `run -f` executes (kevy) or as the SQL `sql compile` turns into
//! the same declarations (sql).

use super::catalog::names;
use super::described::{self, Described, Kind, command_line};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// The two spellings of a declaration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum As {
    Kevy,
    Sql,
}

/// Take `--as kevy|sql` out of `args`; `None` after saying what was wrong.
pub(crate) fn take_as(args: &mut Vec<Vec<u8>>, tool: &str) -> Option<As> {
    let Some(at) = args.iter().position(|a| a == b"--as") else { return Some(As::Kevy) };
    let value = args.get(at + 1).cloned();
    args.drain(at..(at + 2).min(args.len()));
    match value.as_deref() {
        Some(b"kevy") => Some(As::Kevy),
        Some(b"sql") => Some(As::Sql),
        _ => {
            eprint_bytes(&[b"kevy-cli: ", tool.as_bytes(), b": --as takes kevy or sql\n"]);
            None
        }
    }
}

/// `show-create <name> [--as kevy|sql]`; the exit code.
pub(crate) fn show_create(s: &mut Session, args: &[Vec<u8>]) -> u8 {
    let mut args = args.to_vec();
    let Some(spelling) = take_as(&mut args, "show-create") else { return 1 };
    let [name] = args.as_slice() else {
        eprint_bytes(&[b"usage: kevy-cli show-create <table|index|view> [--as kevy|sql]\n"]);
        return 1;
    };
    let d = match described::find(s, name) {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprint_bytes(&[b"kevy-cli: no table, index or view named '", name, b"'\n"]);
            return 1;
        }
        Err(()) => return 1,
    };
    match spelled(&d, name, spelling) {
        Ok(text) => {
            write_out(&text);
            0
        }
        Err(why) => {
            eprint_bytes(&[b"kevy-cli: show-create: ", &why, b"\n"]);
            1
        }
    }
}

/// One object's declaration in `spelling`, newline-terminated.
fn spelled(d: &Described, name: &[u8], spelling: As) -> Result<Vec<u8>, Vec<u8>> {
    let Some(argv) = d.declaration() else {
        let table = d.text(b"table").unwrap_or_default();
        return Err([
            b"index '",
            name,
            b"' is compiled by table '",
            table,
            b"'; show-create ",
            table,
            b" declares it",
        ]
        .concat());
    };
    let kevy = [command_line(&argv), b"\n".to_vec()].concat();
    match (spelling, d.kind) {
        (As::Kevy, _) => Ok(kevy),
        (As::Sql, Kind::Table) => sql_table(&argv),
        (As::Sql, kind) => Err(format!(
            "{} {} has no SQL form here (SQL indexes and views compile from a table); --as kevy prints it",
            if kind == Kind::Index { "an" } else { "a" },
            kind.noun()
        )
        .into_bytes()),
    }
}

fn sql_table(argv: &[Vec<u8>]) -> Result<Vec<u8>, Vec<u8>> {
    let words: Option<Vec<String>> =
        argv.iter().map(|w| String::from_utf8(w.clone()).ok()).collect();
    let words =
        words.ok_or_else(|| b"the declaration is not UTF-8, which SQL text needs".to_vec())?;
    kevy_sql::table_ddl(&words).map(String::into_bytes).map_err(String::into_bytes)
}

/// `dump --schema [--table t]… [--as kevy|sql]`; the exit code. Tables
/// first, then the indexes declared on their own, then the views — the
/// order a replay needs. `--table` narrows it to those tables.
pub(crate) fn dump_schema(s: &mut Session, args: &[Vec<u8>]) -> u8 {
    let mut args = args.to_vec();
    let Some(spelling) = take_as(&mut args, "dump") else { return 1 };
    let Some(tables) = selected_tables(s, &args) else { return 1 };
    match schema_text(s, &tables, spelling, args.is_empty()) {
        Some(text) => {
            write_out(&text);
            0
        }
        None => 1,
    }
}

/// The tables `--table t …` names, or every table when it names none.
fn selected_tables(s: &mut Session, args: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let mut chosen = Vec::new();
    for pair in args.chunks(2) {
        match pair {
            [flag, t] if flag == b"--table" => chosen.push(t.clone()),
            _ => {
                eprint_bytes(&[b"usage: kevy-cli dump --schema [--table t]... [--as kevy|sql]\n"]);
                return None;
            }
        }
    }
    if chosen.is_empty() {
        return names(s, b"TABLE.LIST").or_else(|| unlisted(b"TABLE.LIST"));
    }
    Some(chosen)
}

fn unlisted<T>(verb: &[u8]) -> Option<T> {
    eprint_bytes(&[b"kevy-cli: dump: the server did not answer ", verb, b" with rows\n"]);
    None
}

/// The schema script: `tables`, and with `everything` the bare indexes and
/// the views. In SQL, what has no SQL form is kept as a comment.
pub(crate) fn schema_text(
    s: &mut Session,
    tables: &[Vec<u8>],
    spelling: As,
    everything: bool,
) -> Option<Vec<u8>> {
    let mut out = match spelling {
        As::Kevy => {
            b"# kevy-cli dump --schema: tables, indexes, views; replay with run -f\n".to_vec()
        }
        As::Sql => b"-- kevy-cli dump --schema --as sql; compile with sql compile\n".to_vec(),
    };
    for t in tables {
        out.extend(declared(s, Kind::Table, t, spelling)?);
    }
    if !everything {
        return Some(out);
    }
    for (verb, kind) in [(&b"IDX.LIST"[..], Kind::Index), (b"VIEW.LIST", Kind::View)] {
        for name in names(s, verb).or_else(|| unlisted(verb))? {
            out.extend(declared(s, kind, &name, spelling)?);
        }
    }
    Some(out)
}

/// One object's lines: its declaration, nothing for an index a table
/// compiled, or in SQL a comment for what SQL cannot declare.
fn declared(s: &mut Session, kind: Kind, name: &[u8], spelling: As) -> Option<Vec<u8>> {
    let d = match described::fetch(s, kind, name) {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Listed a moment ago; dropped since.
            eprint_bytes(&[
                b"kevy-cli: dump: ",
                kind.noun().as_bytes(),
                b" '",
                name,
                b"' was dropped while dumping\n",
            ]);
            return None;
        }
        Err(()) => return None,
    };
    let Some(argv) = d.declaration() else { return Some(Vec::new()) };
    match (spelling, kind) {
        (As::Kevy, _) => Some([command_line(&argv), b"\n".to_vec()].concat()),
        (As::Sql, Kind::Table) => match sql_table(&argv) {
            Ok(text) => Some(text),
            Err(why) => {
                eprint_bytes(&[b"kevy-cli: dump: table '", name, b"': ", &why, b"\n"]);
                None
            }
        },
        (As::Sql, _) => {
            Some([&b"-- not carried by SQL: "[..], &command_line(&argv), b"\n"].concat())
        }
    }
}
