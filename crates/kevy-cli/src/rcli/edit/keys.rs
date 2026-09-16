//! Keystrokes from the raw byte stream a terminal sends.
//!
//! The decoder is fed one byte at a time and answers with a key once the
//! bytes seen so far name one, so a key whose escape sequence arrives split
//! across two reads is decoded the same as one that arrives whole.

/// What a keystroke asks the editor to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// A byte to insert — printable, or a control byte with no binding.
    Insert(u8),
    Enter,
    /// Line feed: typed before raw mode was on; it does nothing.
    LineFeed,
    Tab,
    Interrupt,
    Backspace,
    /// Ctrl-D: delete right, or end of input on an empty line.
    DeleteOrEof,
    Delete,
    Transpose,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    HistoryPrev,
    HistoryNext,
    SearchBackward,
    SearchForward,
    /// Ctrl-G: leave incremental search without taking its match.
    Cancel,
    KillLine,
    KillToEnd,
    DeletePrevWord,
    ClearScreen,
    /// An escape sequence with no binding; consumed and ignored.
    Unbound,
}

/// Longest CSI parameter run kept before the sequence is given up on.
const CSI_MAX: usize = 16;

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Escape,
    /// After ESC and a byte that is not `b`, `f`, `[` or `O`.
    EscapeOther,
    /// After `ESC [` or `ESC O`, waiting for the letter.
    Intro(u8),
    /// After `ESC [` and a digit: collecting to a final byte.
    Csi(Vec<u8>),
}

/// The incremental decoder.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    state: State,
}

impl Decoder {
    /// Feed one byte; the key it completes, if any.
    pub(crate) fn feed(&mut self, b: u8) -> Option<Key> {
        match std::mem::take(&mut self.state) {
            State::Ground => self.ground(b),
            State::Escape => match b {
                b'b' => Some(Key::WordLeft),
                b'f' => Some(Key::WordRight),
                b'[' | b'O' => {
                    self.state = State::Intro(b);
                    None
                }
                _ => {
                    self.state = State::EscapeOther;
                    None
                }
            },
            State::EscapeOther => Some(Key::Unbound),
            State::Intro(b'[') if b.is_ascii_digit() => {
                self.state = State::Csi(vec![b]);
                None
            }
            State::Intro(intro) => Some(letter(intro, b)),
            State::Csi(mut params) => {
                params.push(b);
                if (0x40..=0x7e).contains(&b) || params.len() == CSI_MAX - 1 {
                    Some(csi(&params))
                } else {
                    self.state = State::Csi(params);
                    None
                }
            }
        }
    }

    fn ground(&mut self, b: u8) -> Option<Key> {
        Some(match b {
            b'\r' => Key::Enter,
            b'\n' => Key::LineFeed,
            b'\t' => Key::Tab,
            0x03 => Key::Interrupt,
            0x7f | 0x08 => Key::Backspace,
            0x04 => Key::DeleteOrEof,
            0x14 => Key::Transpose,
            0x02 => Key::Left,
            0x06 => Key::Right,
            0x10 => Key::HistoryPrev,
            0x0e => Key::HistoryNext,
            0x12 => Key::SearchBackward,
            0x13 => Key::SearchForward,
            0x07 => Key::Cancel,
            0x15 => Key::KillLine,
            0x0b => Key::KillToEnd,
            0x01 => Key::Home,
            0x05 => Key::End,
            0x0c => Key::ClearScreen,
            0x17 => Key::DeletePrevWord,
            0x1b => {
                self.state = State::Escape;
                return None;
            }
            other => Key::Insert(other),
        })
    }
}

/// `ESC [ <letter>` and `ESC O <letter>`.
fn letter(intro: u8, b: u8) -> Key {
    match (intro, b) {
        (b'[', b'A') => Key::HistoryPrev,
        (b'[', b'B') => Key::HistoryNext,
        (b'[', b'C') => Key::Right,
        (b'[', b'D') => Key::Left,
        (_, b'H') => Key::Home,
        (_, b'F') => Key::End,
        _ => Key::Unbound,
    }
}

/// `ESC [ <digits and parameters> <final>`.
fn csi(params: &[u8]) -> Key {
    match params {
        b"1;5D" | b"1;3D" => Key::WordLeft,
        b"1;5C" | b"1;3C" => Key::WordRight,
        b"3~" => Key::Delete,
        _ => Key::Unbound,
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Key};

    fn decode(bytes: &[u8]) -> Vec<Key> {
        let mut d = Decoder::default();
        bytes.iter().filter_map(|&b| d.feed(b)).collect()
    }

    #[test]
    fn controls() {
        let all =
            b"\r\n\t\x03\x7f\x08\x04\x14\x02\x06\x10\x0e\x12\x13\x07\x15\x0b\x01\x05\x0c\x17a\x0f";
        use Key::*;
        assert_eq!(
            decode(all),
            [
                Enter,
                LineFeed,
                Tab,
                Interrupt,
                Backspace,
                Backspace,
                DeleteOrEof,
                Transpose,
                Left,
                Right,
                HistoryPrev,
                HistoryNext,
                SearchBackward,
                SearchForward,
                Cancel,
                KillLine,
                KillToEnd,
                Home,
                End,
                ClearScreen,
                DeletePrevWord,
                Insert(b'a'),
                Insert(0x0f)
            ]
        );
    }

    #[test]
    fn escape_sequences() {
        use Key::*;
        let seqs: &[(&[u8], Key)] = &[
            (b"\x1bb", WordLeft),
            (b"\x1bf", WordRight),
            (b"\x1b[A", HistoryPrev),
            (b"\x1b[B", HistoryNext),
            (b"\x1b[C", Right),
            (b"\x1b[D", Left),
            (b"\x1b[H", Home),
            (b"\x1b[F", End),
            (b"\x1bOH", Home),
            (b"\x1bOF", End),
            (b"\x1b[1;5D", WordLeft),
            (b"\x1b[1;3D", WordLeft),
            (b"\x1b[1;5C", WordRight),
            (b"\x1b[1;3C", WordRight),
            (b"\x1b[3~", Delete),
            (b"\x1b[2~", Unbound),
            (b"\x1bOP", Unbound),
            (b"\x1b[Z", Unbound),
            (b"\x1bxy", Unbound),
        ];
        for (bytes, key) in seqs {
            assert_eq!(decode(bytes), [*key], "{bytes:?}");
        }
        // An overlong parameter run is given up on rather than held forever.
        // 19 digits: the sequence gives up holding at 15 bytes, and the rest
        // are ordinary input again.
        assert_eq!(
            decode(b"\x1b[1111111111111111111a"),
            [Unbound, Insert(b'1'), Insert(b'1'), Insert(b'1'), Insert(b'1'), Insert(b'a')]
        );
    }

    #[test]
    fn a_sequence_split_across_reads_decodes_whole() {
        let mut d = Decoder::default();
        assert_eq!([d.feed(0x1b), d.feed(b'['), d.feed(b'1'), d.feed(b';')], [None; 4]);
        assert_eq!(d.feed(b'5'), None);
        assert_eq!(d.feed(b'C'), Some(Key::WordRight));
        assert_eq!(d.feed(b'x'), Some(Key::Insert(b'x')));
    }
}
