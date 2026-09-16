//! The REPL loop: read a line, act on it, and between lines wait out
//! pub/sub traffic.

use super::cnum::strtoll;
use super::docs::model::Docs;
use super::edit::editor::{Assist, Outcome};
use super::format::Output;
use super::input::Input;
use super::send::{Read, write_out};
use super::session::{Connect, Session};
use super::splitargs::split_args;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

/// Run the REPL until the input ends or `quit`; the process exit code.
pub(crate) fn run(s: &mut Session) -> u8 {
    s.interactive = true;
    let mut input = Input::open();
    // Hints and completion are for a person at a prompt; a pipe never asks.
    let docs = input.keeps_history().then(|| s.docs());
    if input.keeps_history() {
        super::help::load_preferences();
    }
    let hint = |line: &[u8]| docs.as_deref().and_then(|d| hint_shown(d, line));
    let complete = |line: &[u8]| docs.as_deref().map_or_else(Vec::new, |d| d.completions(line));
    let assist = Assist { hint: &hint, complete: &complete };
    loop {
        let prompt = super::prompt::prompt(s);
        match input.read(&prompt, &assist) {
            Outcome::Line(line) => {
                if !line.is_empty()
                    && let Some(code) = handle_line(s, &mut input, &line)
                {
                    return code;
                }
            }
            Outcome::Eof | Outcome::Interrupted => {
                if s.pubsub_mode {
                    s.pubsub_mode = false;
                    if s.connect(Connect::Report) {
                        continue;
                    }
                }
                return 0;
            }
        }
        if s.pubsub_mode {
            wait_for_messages_or_stdin(s);
        }
    }
}

/// The hint as the editor shows it: after a space unless the line ends in one.
fn hint_shown(docs: &Docs, line: &[u8]) -> Option<Vec<u8>> {
    if !super::session::hints_on() {
        return None;
    }
    let hint = docs.hint(line)?;
    Some(match line.last() {
        Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') => hint,
        _ => [b" ".as_slice(), &hint].concat(),
    })
}

/// One non-empty line. `Some(code)` ends the program.
fn handle_line(s: &mut Session, input: &mut Input, line: &[u8]) -> Option<u8> {
    let Some(argv) = split_args(line) else {
        write_out(b"Invalid argument(s)\n");
        input.remember(line, &[]);
        return None;
    };
    if argv.is_empty() {
        return None;
    }
    let (value, used) = strtoll(&argv[0]);
    let (repeat, skip) = if argv.len() > 1 && used == argv[0].len() {
        // `strtol` sets ERANGE on overflow; `strtoll` here saturates instead.
        if value <= 0 || value == i64::MAX {
            write_out(b"Invalid kevy-cli repeat command option value.\n");
            return None;
        }
        (value, 1)
    } else {
        (1, 0)
    };
    input.remember(line, &argv[skip..]);
    run_line(s, &argv, repeat, skip)
}

/// The line's action: a REPL word, or the command, timed.
fn run_line(s: &mut Session, argv: &[Vec<u8>], repeat: i64, skip: usize) -> Option<u8> {
    let word = |w: &str| argv[0].eq_ignore_ascii_case(w.as_bytes());
    if word("quit") || word("exit") {
        return Some(0);
    }
    if argv[0].first() == Some(&b':') {
        super::help::preference(argv, false);
    } else if word("restart") {
        write_out(b"Use 'restart' only in Lua debugging mode.\n");
    } else if argv.len() == 3 && word("connect") {
        s.opts.host = argv[1].clone();
        s.opts.port = super::cnum::atoi(&argv[2]);
        s.connect(Connect::Report);
    } else if argv.len() == 1 && word("clear") {
        write_out(b"\x1b[H\x1b[2J");
    } else {
        let started = Instant::now();
        s.issue(&argv[skip..], repeat);
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(500) && s.opts.output == Output::Standard {
            write_out(format!("({:.2}s)\n", elapsed.as_millis() as f64 / 1000.0).as_bytes());
        }
    }
    None
}

/// While subscribed: print messages until the user types something.
fn wait_for_messages_or_stdin(s: &mut Session) {
    let show_info = s.opts.output != Output::Raw
        && (std::io::stdout().is_terminal() || std::env::var_os("FAKETTY").is_some());
    let color = show_info && std::env::var("TERM").is_ok_and(|t| t.contains("xterm"));
    while s.pubsub_mode {
        if !drain_buffered(s) {
            s.print_context_error();
            std::process::exit(1);
        }
        let Some(fd) = s.conn.as_ref().map(super::conn::Conn::fd) else { return };
        if show_info {
            let info = b"Reading messages... (press Ctrl-C to quit or any key to type command)\r";
            if color {
                write_out(&[b"\x1b[1;90m", &info[..], b"\x1b[0m"].concat());
            } else {
                write_out(info);
            }
        }
        let ready = kevy_sys::wait_readable(&[fd, 0], Duration::from_secs(5))
            .unwrap_or_else(|_| vec![false, true]);
        if show_info {
            write_out(b"\x1b[K");
        }
        if ready[0] {
            if let Read::Failed = s.read_reply(false) {
                s.print_context_error();
                std::process::exit(1);
            }
        } else if ready[1] {
            return;
        }
    }
}

/// Print replies already in the buffer; `false` on a protocol error.
fn drain_buffered(s: &mut Session) -> bool {
    loop {
        let Some(conn) = s.conn.as_mut() else { return true };
        match conn.buffered_reply() {
            Ok(Some((reply, texts))) => {
                let out =
                    super::format::render(&reply, &texts, s.opts.output, &s.opts.delims, false);
                write_out(&out);
            }
            Ok(None) => return true,
            Err(e) => {
                s.link_error = Some(e);
                return false;
            }
        }
    }
}
