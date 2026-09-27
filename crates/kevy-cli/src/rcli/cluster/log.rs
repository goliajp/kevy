//! The cluster manager's log: each line has a level, shown as a colour when
//! the terminal type names an xterm.

use crate::rcli::send::write_out;

/// How a log line is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    /// Progress (`>>>`): bold.
    Info,
    /// Yellow.
    Warn,
    /// Red.
    Err,
    /// Green.
    Ok,
}

fn code(level: Level) -> &'static [u8] {
    match level {
        Level::Info => b"\x1b[29;1m",
        Level::Warn => b"\x1b[33;1m",
        Level::Err => b"\x1b[31;1m",
        Level::Ok => b"\x1b[32;1m",
    }
}

/// `text` and a newline, coloured as a whole (the reset follows the newline).
pub(crate) fn line(color: bool, level: Level, text: &[u8]) {
    if color {
        write_out(&[code(level), text, b"\n\x1b[0m"].concat());
    } else {
        write_out(&[text, b"\n"].concat());
    }
}

/// `head` coloured, then `rest` plain: `>>> Calling` and its command.
pub(crate) fn head(color: bool, level: Level, head: &[u8], rest: &[u8]) {
    if color {
        write_out(&[code(level), head, b"\x1b[0m", rest].concat());
    } else {
        write_out(&[head, rest].concat());
    }
}

/// A plain line.
pub(crate) fn plain(text: &[u8]) {
    write_out(&[text, b"\n"].concat());
}
