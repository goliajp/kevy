//! `run [-f file]… [-c command]… [--force] [--echo] [--atomic]`: commands
//! from files and the command line, in order, stopping at the first error
//! reply unless told otherwise.
//!
//! Exit codes, as psql's: 0 everything ran, 1 this client failed (a file
//! could not be read, a line could not be split), 2 the connection was lost,
//! 3 an error reply stopped the script (or, with --force, happened at all).

use super::options::Common;
use crate::rcli::format::render;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// A script's settings.
struct Script {
    sources: Vec<Source>,
    force: bool,
    echo: bool,
    atomic: bool,
}

enum Source {
    File(Vec<u8>),
    Command(Vec<u8>),
}

/// Run `run`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some(script) = parse(&common.args) else { return 1 };
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for source in &script.sources {
        match source {
            Source::Command(c) => lines.push(c.clone()),
            Source::File(path) => {
                match std::fs::read(std::ffi::OsStr::new(&*String::from_utf8_lossy(path))) {
                    Ok(text) => lines.extend(
                        text.split(|&b| b == b'\n')
                            .map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec()),
                    ),
                    Err(e) => {
                        let why = crate::rcli::conn::strerror(&e);
                        eprint_bytes(&[
                            b"kevy-cli: cannot read '",
                            path,
                            b"': ",
                            why.as_bytes(),
                            b"\n",
                        ]);
                        return 1;
                    }
                }
            }
        }
    }
    if script.atomic {
        eprint_bytes(&[b"kevy-cli: --atomic sends the script inside MULTI/EXEC; a command that fails does not undo the others\n"]);
        lines.insert(0, b"MULTI".to_vec());
        lines.push(b"EXEC".to_vec());
    }
    execute(s, &script, &lines)
}

fn execute(s: &mut Session, script: &Script, lines: &[Vec<u8>]) -> u8 {
    let mut failed = false;
    for line in lines.iter().filter(|l| !l.trim_ascii().is_empty() && !l.starts_with(b"#")) {
        let Some(argv) = crate::rcli::splitargs::split_args(line) else {
            eprint_bytes(&[b"kevy-cli: cannot split '", line, b"' (unbalanced quotes)\n"]);
            return 1;
        };
        if script.echo {
            write_out(&[line.as_slice(), b"\n"].concat());
        }
        let words: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
        let reply = match s.request(&words) {
            Ok(reply) => reply,
            Err(e) => {
                eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
                return 2;
            }
        };
        write_out(&render(&reply, &[], s.opts.output, &s.opts.delims, false));
        if matches!(reply, Reply::Error(_) | Reply::BlobError(_)) {
            failed = true;
            if !script.force {
                return 3;
            }
        }
    }
    if failed { 3 } else { 0 }
}

fn parse(args: &[Vec<u8>]) -> Option<Script> {
    let mut script = Script { sources: Vec::new(), force: false, echo: false, atomic: false };
    let mut i = 0;
    while i < args.len() {
        match (args[i].as_slice(), args.get(i + 1)) {
            (b"-f", Some(v)) => {
                script.sources.push(Source::File(v.clone()));
                i += 1;
            }
            (b"-c", Some(v)) => {
                script.sources.push(Source::Command(v.clone()));
                i += 1;
            }
            (b"--force", _) => script.force = true,
            (b"--echo", _) => script.echo = true,
            (b"--atomic", _) => script.atomic = true,
            (other, _) => {
                eprint_bytes(&[
                    b"kevy-cli: run: unexpected '",
                    other,
                    b"' (-f file, -c command, --force, --echo, --atomic)\n",
                ]);
                return None;
            }
        }
        i += 1;
    }
    if script.sources.is_empty() {
        eprint_bytes(&[b"kevy-cli: run needs -f file or -c command\n"]);
        return None;
    }
    Some(script)
}
