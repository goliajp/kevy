//! `dump --all <dir>` and `load <dir>`: a relational dump as files.
//!
//! The directory holds `schema.kevy` (the declarations, replayable with
//! `run -f`), one `table-N.csv` per table (its rows, key first, as
//! `export-csv` writes them) and `tables`, a line per table naming its file.
//! Only rows under a table's prefix, and of those only the declared columns,
//! are carried — a CSV has one set of columns, and a field a row holds
//! beyond them is not part of the table. `kevy-cli export` carries the whole
//! keyspace byte for byte.
//!
//! `load` works in the order that costs least: the rows, then the
//! declarations (so each index backfills once instead of updating per
//! row), then waits for every index, then runs doctor.

use super::catalog::names;
use super::described::{self, Kind, command_line};
use super::export_csv::{self, Plan};
use super::options::{self, Common};
use super::schema::{As, schema_text};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "schema.kevy";
const TABLES: &str = "tables";

fn fail(parts: &[&[u8]]) -> u8 {
    eprint_bytes(&[b"kevy-cli: ", &parts.concat(), b"\n"]);
    1
}

fn dir_of(args: &[Vec<u8>], tool: &str) -> Option<PathBuf> {
    match args {
        [dir] => Some(PathBuf::from(String::from_utf8_lossy(dir).into_owned())),
        _ => {
            eprint_bytes(&[b"usage: kevy-cli ", tool.as_bytes(), b" <dir>\n"]);
            None
        }
    }
}

/// `dump --all <dir>`; the exit code. The directory must be new or empty.
pub(crate) fn dump_all(s: &mut Session, args: &[Vec<u8>]) -> u8 {
    let Some(dir) = dir_of(args, "--kevy dump --all") else { return 1 };
    if let Err(why) = empty_dir(&dir) {
        return fail(&[b"dump: ", why.as_bytes()]);
    }
    let Some(tables) = names(s, b"TABLE.LIST") else {
        return fail(&[b"dump: the server did not answer TABLE.LIST with rows"]);
    };
    let Some(schema) = schema_text(s, &tables, As::Kevy, true) else { return 1 };
    let mut listing = Vec::new();
    for (n, table) in tables.iter().enumerate() {
        let file = format!("table-{}.csv", n + 1);
        listing.extend(command_line(&[file.clone().into_bytes(), table.clone()]));
        listing.push(b'\n');
        let code = dump_table(s, table, &dir.join(&file));
        if code != 0 {
            return code;
        }
    }
    for (name, bytes) in [(SCHEMA, &schema), (TABLES, &listing)] {
        if let Err(e) = std::fs::write(dir.join(name), bytes) {
            return fail(&[
                b"dump: cannot write ",
                name.as_bytes(),
                b": ",
                e.to_string().as_bytes(),
            ]);
        }
    }
    let shown = dir.display().to_string();
    write_out(format!("dumped {} table(s) to {shown}\n", tables.len()).as_bytes());
    0
}

fn empty_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    match entries.next() {
        None => Ok(()),
        Some(_) => Err(format!("{} is not empty; a dump goes into a new directory", dir.display())),
    }
}

/// One table's rows: every declared column, keys from SCAN on its prefix.
fn dump_table(s: &mut Session, table: &[u8], path: &Path) -> u8 {
    let d = match described::fetch(s, Kind::Table, table) {
        Ok(Some(d)) => d,
        Ok(None) => return fail(&[b"dump: table '", table, b"' was dropped while dumping"]),
        Err(()) => return 1,
    };
    let prefix = d.text(b"prefix").unwrap_or_default();
    let columns = d.columns().into_iter().map(|(c, _)| c).collect();
    let file = path.to_string_lossy().into_owned().into_bytes();
    export_csv::export(s, &Plan::scan(prefix, columns, file))
}

/// `load <dir>`; the exit code of the first step that fails.
pub(crate) fn load(s: &mut Session, args: &[Vec<u8>], common: &Common) -> u8 {
    let Some(dir) = dir_of(args, "--kevy load") else { return 1 };
    let read = |name: &str| std::fs::read(dir.join(name));
    let (Ok(schema), Ok(listing)) = (read(SCHEMA), read(TABLES)) else {
        let shown = dir.display().to_string();
        return fail(&[
            b"load: ",
            shown.as_bytes(),
            b" holds no dump (schema.kevy and tables, as dump --all writes them)",
        ]);
    };
    let code = import_rows(s, common, &dir, &schema, &listing);
    if code != 0 {
        return code;
    }
    let schema_path = dir.join(SCHEMA).to_string_lossy().into_owned();
    let code = step(s, common, &[b"-f", schema_path.as_bytes()], super::script::run);
    if code != 0 {
        return code;
    }
    let code = step(s, common, &[b"--all"], super::wait_ready::run);
    if code != 0 {
        return code;
    }
    doctor(s)
}

/// Each table's rows, keys as dumped, before anything is declared.
fn import_rows(s: &mut Session, common: &Common, dir: &Path, schema: &[u8], listing: &[u8]) -> u8 {
    for line in listing.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let words = crate::rcli::splitargs::split_args(line);
        let Some([file, table]) = words.and_then(|w| <[Vec<u8>; 2]>::try_from(w).ok()) else {
            return fail(&[b"load: a line of 'tables' is not <file> <table>: ", line]);
        };
        let Some(prefix) = declared_prefix(schema, &table) else {
            return fail(&[b"load: schema.kevy does not declare table '", &table, b"'"]);
        };
        let path = dir.join(String::from_utf8_lossy(&file).as_ref());
        let path = path.to_string_lossy().into_owned();
        let args: [&[u8]; 6] =
            [path.as_bytes(), b"--prefix", &prefix, b"--key-column", b"key", b"--header"];
        let code = step(s, common, &args, super::import_csv::run);
        if code != 0 {
            return code;
        }
    }
    0
}

/// Run one tool with `args` and the caller's output options.
fn step(
    s: &mut Session,
    common: &Common,
    args: &[&[u8]],
    tool: fn(&mut Session, &Common) -> u8,
) -> u8 {
    let args: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let Some(mut c) = options::parse(&args, s.opts.output) else { return 1 };
    c.style = common.style.clone();
    tool(s, &c)
}

/// The `PREFIX` of `table`'s `TABLE.DECLARE` line in the schema script.
fn declared_prefix(schema: &[u8], table: &[u8]) -> Option<Vec<u8>> {
    schema.split(|&b| b == b'\n').find_map(|line| {
        let words = crate::rcli::splitargs::split_args(line)?;
        let is_it = words.first()?.eq_ignore_ascii_case(b"TABLE.DECLARE") && words.get(1)? == table;
        is_it.then(|| words.get(3).cloned()).flatten()
    })
}

/// doctor over TCP, checking tables, bare indexes and views.
fn doctor(s: &Session) -> u8 {
    if s.opts.socket.is_some() {
        eprint_bytes(&[b"kevy-cli: load: doctor connects over TCP only; not run over a socket (run doctor -h <host> -p <port>)\n"]);
        return 0;
    }
    let host = String::from_utf8_lossy(&s.opts.host).into_owned();
    let Ok(port) = u16::try_from(s.opts.port) else {
        return fail(&[b"load: the port is out of range for doctor"]);
    };
    let mut client = match kevy_resp_client::RespClient::connect(&host, port) {
        Ok(c) => c,
        Err(e) => return fail(&[b"load: doctor could not connect: ", e.to_string().as_bytes()]),
    };
    let scope = crate::doctor::Scope { indexes: true, views: true };
    match crate::doctor::run_scoped(&mut client, false, scope) {
        Ok(code) if code == std::process::ExitCode::SUCCESS => 0,
        Ok(_) => 3,
        Err(e) => fail(&[b"load: doctor: ", e.to_string().as_bytes()]),
    }
}
