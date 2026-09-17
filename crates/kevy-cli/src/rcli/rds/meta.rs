//! The REPL's backslash commands: the relational tools under psql's names,
//! and the two display switches. No Redis command starts with a backslash,
//! so none is shadowed.

use super::route::{Tool, run_tool};
use crate::rcli::send::write_out;
use crate::rcli::session::Session;

/// Display switches that last for the session.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Prefs {
    /// `\x`: one block per record.
    pub(crate) expanded: bool,
    /// `\timing`: how long each command took.
    pub(crate) timing: bool,
}

const HELP: &[u8] = b"\\dt [pattern]      tables\n\\di [table|pattern] indexes\n\\dv [pattern]      views\n\\d  name           describe a table, index or view\n\\d+ name           describe, with VERIFY\n\\query [--all] ... a query's rows\n\\explain ...       a query's plan (--analyze to run it)\n\\advise            paths refused queries asked for\n\\i file            run the commands in a file\n\\watch s [n] cmd   run a command every s seconds\n\\conninfo          the connection and the server\n\\x                 expanded display on/off\n\\timing            timing on/off\n";

/// Run the backslash command on `line` (the backslash included).
pub(crate) fn run(s: &mut Session, line: &[u8]) {
    let Some(words) = crate::rcli::splitargs::split_args(&line[1..]) else {
        write_out(b"Invalid argument(s)\n");
        return;
    };
    let Some((name, args)) = words.split_first() else {
        write_out(b"Invalid command \\. Try \\? for help.\n");
        return;
    };
    let tool = match name.as_slice() {
        b"dt" => Tool::Tables,
        b"di" => Tool::Indexes,
        b"dv" => Tool::Views,
        b"d" => Tool::Describe,
        b"d+" => Tool::DescribePlus,
        b"query" => Tool::Query,
        b"explain" => Tool::Explain,
        b"advise" => Tool::Advise,
        b"watch" => Tool::Watch,
        b"conninfo" => Tool::Status,
        b"i" => return file(s, args),
        b"x" => return toggle(&mut s.rds.expanded, b"Expanded display"),
        b"timing" => return toggle(&mut s.rds.timing, b"Timing"),
        b"?" => return write_out(HELP),
        _ => {
            return write_out(
                &[&b"Invalid command \\"[..], name, b". Try \\? for help.\n"].concat(),
            );
        }
    };
    run_tool(s, tool, args);
}

fn file(s: &mut Session, args: &[Vec<u8>]) {
    let Some(path) = args.first() else {
        write_out(b"\\i: missing required argument\n");
        return;
    };
    run_tool(s, Tool::Run, &[b"-f".to_vec(), path.clone()]);
}

fn toggle(flag: &mut bool, what: &[u8]) {
    *flag = !*flag;
    write_out(&[what, if *flag { &b" is on.\n"[..] } else { b" is off.\n" }].concat());
}
