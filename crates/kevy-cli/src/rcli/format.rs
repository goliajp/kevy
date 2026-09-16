//! Reply rendering: which formatter a reply goes through, and the
//! commands whose replies are printed raw whatever the output mode.
//!
//! The formatters are ports of redis-cli 8.10.1's `cliFormatReplyTTY`,
//! `cliFormatReplyRaw`, `cliFormatReplyCSV` and `cliFormatReplyJson`. Where
//! redis-cli is wrong they are not ports, and each such place names its
//! deviation id from `bench/cligate/deviations.txt`.

use kevy_resp::Reply;

pub(crate) use super::fmt_flat::{csv, raw};
pub(crate) use super::fmt_json::json;
pub(crate) use super::fmt_tty::{invalidate_tty, is_invalidate, tty};

/// The output mode: redis-cli's `OUTPUT_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Output {
    /// Numbered, quoted, typed — the default on a terminal.
    Standard,
    /// Bytes as sent — the default when stdout is not a terminal.
    Raw,
    /// `--csv`.
    Csv,
    /// `--json`.
    Json,
    /// `--quoted-json`.
    QuotedJson,
}

/// The wire text of the doubles in one reply, consumed in walk order.
///
/// A reply parsed without the text (never the case for replies this CLI
/// reads, but the type does not know that) falls back to Rust's rendering.
pub(crate) struct Doubles<'a>(std::slice::Iter<'a, Vec<u8>>);

impl<'a> Doubles<'a> {
    pub(crate) fn new(texts: &'a [Vec<u8>]) -> Self {
        Doubles(texts.iter())
    }

    pub(crate) fn next_text(&mut self, value: f64) -> Vec<u8> {
        match self.0.next() {
            Some(text) => text.clone(),
            None => value.to_string().into_bytes(),
        }
    }
}

/// `cliFormatReply`: render one reply in `mode`, with the trailing
/// delimiter each mode adds. `verbatim` is the raw-regardless list.
pub(crate) fn render(
    reply: &Reply,
    texts: &[Vec<u8>],
    mode: Output,
    delims: &Delims,
    verbatim: bool,
) -> Vec<u8> {
    let mut d = Doubles::new(texts);
    let mut out = Vec::new();
    if verbatim {
        raw(reply, &mut d, &delims.multibulk, &mut out);
        return out;
    }
    match mode {
        Output::Standard => tty(reply, b"", &mut d, &mut out),
        Output::Raw => {
            raw(reply, &mut d, &delims.multibulk, &mut out);
            out.extend_from_slice(&delims.reply);
        }
        Output::Csv => {
            csv(reply, &mut d, &mut out);
            out.push(b'\n');
        }
        Output::Json | Output::QuotedJson => {
            json(reply, &mut d, mode, &mut out);
            out.push(b'\n');
        }
    }
    out
}

/// `-d` (between elements of an aggregate) and `-D` (after each reply).
#[derive(Clone, Debug)]
pub(crate) struct Delims {
    pub(crate) multibulk: Vec<u8>,
    pub(crate) reply: Vec<u8>,
}

/// The commands whose reply redis-cli prints raw in every output mode
/// (`cliSendCommand`, rc:2461-2489): their payload is already text meant
/// for a human, and quoting it would turn line breaks into `\r\n`.
pub(crate) fn is_verbatim_command(argv: &[Vec<u8>]) -> bool {
    let is =
        |i: usize, word: &str| argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(word.as_bytes()));
    let n = argv.len();
    is(0, "info")
        || is(0, "lolwut")
        || (n >= 2
            && is(0, "debug")
            && (is(1, "htstats") || is(1, "htstats-key") || is(1, "client-eviction")))
        || (n >= 2 && is(0, "memory") && (is(1, "malloc-stats") || is(1, "doctor")))
        || (n == 2 && is(0, "cluster") && (is(1, "nodes") || is(1, "info")))
        || (n >= 2 && is(0, "client") && (is(1, "list") || is(1, "info")))
        || (n == 3 && is(0, "latency") && is(1, "graph"))
        || (n == 2 && is(0, "latency") && is(1, "doctor"))
        || (n >= 2 && is(0, "proxy") && is(1, "info"))
}

/// A C string's view of `bytes`: everything before the first NUL. redis-cli
/// prints errors and statuses with `%s`, which stops there.
pub(crate) fn c_str(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
}
