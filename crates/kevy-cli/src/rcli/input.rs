//! Where REPL lines come from: a pipe, a terminal that cannot take escape
//! sequences, or a terminal with the line editor.

use super::edit::editor::{Assist, Outcome, no_assist, read_line};
use super::edit::history::{History, history_path, is_sensitive};
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Terminal types that do not understand the editor's escape sequences.
const DUMB_TERMS: &[&str] = &["dumb", "cons25", "emacs"];

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Standard input is not a terminal: lines, no prompt, no history.
    Pipe,
    /// A terminal without escape sequences: a prompt, then a plain line.
    Plain,
    /// The line editor. `raw` is false under `FAKETTY_WITH_PROMPT`, which
    /// edits a piped stdin that has no terminal mode to change.
    Edit { raw: bool },
}

/// The REPL's line source, with the history it keeps.
pub(crate) struct Input {
    mode: Mode,
    history: History,
    history_file: Option<PathBuf>,
}

impl Input {
    /// Pick the mode from the environment, and load history when the mode
    /// keeps one.
    pub(crate) fn open() -> Input {
        let mut input =
            Input { mode: Mode::detect(), history: History::default(), history_file: None };
        if input.keeps_history() {
            input.history_file = history_path(|k: &str| std::env::var_os(k));
            if let Some(path) = &input.history_file {
                input.history.load(path);
            }
        }
        input
    }

    /// Standard input is a real terminal (not `FAKETTY_WITH_PROMPT`): only
    /// then is the command reference fetched for hints and completion.
    pub(crate) fn on_a_terminal(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    /// A password, typed at `prompt` with every character shown as `*` when
    /// there is a prompt at all. `None` at end of input or Ctrl-C.
    pub(crate) fn read_secret(prompt: &[u8]) -> Option<Vec<u8>> {
        let outcome = match Mode::detect() {
            Mode::Pipe => plain_line(),
            Mode::Plain => {
                super::send::write_out(prompt);
                plain_line()
            }
            Mode::Edit { raw } => {
                edit_line(prompt, &History::default(), &no_assist(), raw, Echo::Masked)
            }
        };
        match outcome {
            Outcome::Line(line) => Some(line),
            Outcome::Eof | Outcome::Interrupted => None,
        }
    }

    /// History, and the preferences file, are for a person at a prompt.
    pub(crate) fn keeps_history(&self) -> bool {
        self.mode != Mode::Pipe
    }

    /// Read one line.
    pub(crate) fn read(&mut self, prompt: &[u8], assist: &Assist<'_>) -> Outcome {
        match self.mode {
            Mode::Pipe => plain_line(),
            Mode::Plain => {
                super::send::write_out(prompt);
                plain_line()
            }
            Mode::Edit { raw } => self.edit(prompt, assist, raw),
        }
    }

    fn edit(&mut self, prompt: &[u8], assist: &Assist<'_>, raw: bool) -> Outcome {
        edit_line(prompt, &self.history, assist, raw, Echo::Plain)
    }

    /// Record a line the user entered. `argv` is the command it ran (after a
    /// repeat count), which decides whether the line may reach the file.
    pub(crate) fn remember(&mut self, line: &[u8], argv: &[Vec<u8>]) {
        if !self.keeps_history() {
            return;
        }
        let sensitive = is_sensitive(argv);
        self.history.add(line, sensitive);
        if !sensitive && let Some(path) = &self.history_file {
            // A history file that cannot be written is not a reason to stop
            // the session; the line is still in memory.
            let _ = self.history.save(path);
        }
    }
}

impl Mode {
    fn detect() -> Mode {
        let stdin_tty = std::io::stdin().is_terminal();
        let faketty = std::env::var_os("FAKETTY_WITH_PROMPT").is_some();
        let dumb = std::env::var("TERM")
            .is_ok_and(|t| DUMB_TERMS.iter().any(|d| t.eq_ignore_ascii_case(d)));
        match (stdin_tty || faketty, dumb) {
            (false, _) => Mode::Pipe,
            (true, true) => Mode::Plain,
            (true, false) => Mode::Edit { raw: stdin_tty && !faketty },
        }
    }
}

/// Whether typed characters are shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Echo {
    Plain,
    Masked,
}

/// One line through the editor, in raw mode when there is a terminal to set.
fn edit_line(
    prompt: &[u8],
    history: &History,
    assist: &Assist<'_>,
    raw: bool,
    echo: Echo,
) -> Outcome {
    let _raw = raw.then(|| kevy_sys::RawMode::enable(0).ok()).flatten();
    let cols = if raw { kevy_sys::terminal_columns(1).map_or(80, usize::from) } else { 80 };
    let mut stdout = std::io::stdout().lock();
    let mut typed = typed();
    let Some(stdin) = typed.as_mut() else { return Outcome::Eof };
    read_line(stdin, &mut stdout, prompt, history, assist, cols, echo == Echo::Masked)
        .unwrap_or(Outcome::Eof)
}

/// A line from a non-editing source: bytes to `\n`, which is dropped. End of
/// input with nothing read is the end.
fn plain_line() -> Outcome {
    let mut line = Vec::new();
    let mut typed = typed();
    let Some(stdin) = typed.as_mut() else { return Outcome::Eof };
    match stdin.read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => Outcome::Eof,
        Ok(_) => {
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            let _ = std::io::stdout().flush(); // the prompt, if any, is already out
            Outcome::Line(line)
        }
    }
}

/// Standard input as every REPL line is read from it: one buffer, so the
/// subscribed wait can see bytes typed or pasted ahead that are already out of
/// the descriptor. `None` when standard input is closed.
static TYPED: OnceLock<Mutex<Option<BufReader<File>>>> = OnceLock::new();

fn typed() -> MutexGuard<'static, Option<BufReader<File>>> {
    TYPED
        .get_or_init(|| {
            let fd = std::io::stdin().as_fd().try_clone_to_owned().ok();
            Mutex::new(fd.map(|fd| BufReader::new(File::from(fd))))
        })
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The rest of standard input (`-x` / `-X`), after anything already read
/// from it, such as an `--askpass` line.
pub(crate) fn read_all_typed() -> std::io::Result<Vec<u8>> {
    let mut all = Vec::new();
    if let Some(stdin) = typed().as_mut() {
        std::io::Read::read_to_end(stdin, &mut all)?;
    }
    Ok(all)
}

/// Bytes already read from standard input and not yet part of a line.
pub(crate) fn typed_ahead() -> bool {
    typed().as_ref().is_some_and(|r| !r.buffer().is_empty())
}
