//! The line being edited: bytes and a cursor, and every edit a key can make.
//!
//! Each operation is at worst one shift of the bytes after the cursor.

/// Longest line the editor accepts, in bytes; typing past it is ignored.
pub(crate) const MAX_LINE: usize = 4095;

/// Bytes plus a cursor that is always within `0..=len`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Line {
    buf: Vec<u8>,
    pos: usize,
}

/// Letters, digits and `_` make up a word for word-wise cursor motion.
fn is_word(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

impl Line {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub(crate) fn cursor(&self) -> usize {
        self.pos
    }

    /// Replace the whole line, cursor at the end (history recall).
    pub(crate) fn set(&mut self, bytes: &[u8]) {
        self.buf.clear();
        self.buf.extend_from_slice(&bytes[..bytes.len().min(MAX_LINE)]);
        self.pos = self.buf.len();
    }

    /// Insert at the cursor; `false` when the line is full.
    pub(crate) fn insert(&mut self, b: u8) -> bool {
        if self.buf.len() >= MAX_LINE {
            return false;
        }
        self.buf.insert(self.pos, b);
        self.pos += 1;
        true
    }

    pub(crate) fn backspace(&mut self) {
        if self.pos > 0 {
            self.pos -= 1;
            self.buf.remove(self.pos);
        }
    }

    pub(crate) fn delete(&mut self) {
        if self.pos < self.buf.len() {
            self.buf.remove(self.pos);
        }
    }

    /// Swap the bytes either side of the cursor and step past them, except at
    /// the last byte, where the cursor stays.
    pub(crate) fn transpose(&mut self) {
        if self.pos > 0 && self.pos < self.buf.len() {
            self.buf.swap(self.pos - 1, self.pos);
            if self.pos != self.buf.len() - 1 {
                self.pos += 1;
            }
        }
    }

    pub(crate) fn left(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    pub(crate) fn right(&mut self) {
        self.pos = (self.pos + 1).min(self.buf.len());
    }

    pub(crate) fn home(&mut self) {
        self.pos = 0;
    }

    pub(crate) fn end(&mut self) {
        self.pos = self.buf.len();
    }

    /// Back over non-word bytes, then over the word before them.
    pub(crate) fn word_left(&mut self) {
        while self.pos > 0 && !is_word(self.buf[self.pos - 1]) {
            self.pos -= 1;
        }
        while self.pos > 0 && is_word(self.buf[self.pos - 1]) {
            self.pos -= 1;
        }
    }

    /// Forward over the current word, then over the non-word bytes after it.
    pub(crate) fn word_right(&mut self) {
        while self.pos < self.buf.len() && is_word(self.buf[self.pos]) {
            self.pos += 1;
        }
        while self.pos < self.buf.len() && !is_word(self.buf[self.pos]) {
            self.pos += 1;
        }
    }

    pub(crate) fn kill_line(&mut self) {
        self.buf.clear();
        self.pos = 0;
    }

    pub(crate) fn kill_to_end(&mut self) {
        self.buf.truncate(self.pos);
    }

    /// Ctrl-W: back over spaces, then over non-spaces, deleting both. Words
    /// here are space-separated, unlike cursor motion's.
    pub(crate) fn delete_prev_word(&mut self) {
        let end = self.pos;
        while self.pos > 0 && self.buf[self.pos - 1] == b' ' {
            self.pos -= 1;
        }
        while self.pos > 0 && self.buf[self.pos - 1] != b' ' {
            self.pos -= 1;
        }
        self.buf.drain(self.pos..end);
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, MAX_LINE};

    fn line(text: &str, pos: usize) -> Line {
        let mut l = Line::default();
        l.set(text.as_bytes());
        l.pos = pos;
        l
    }

    fn show(l: &Line) -> (String, usize) {
        (String::from_utf8_lossy(l.bytes()).into_owned(), l.cursor())
    }

    #[test]
    fn inserting_and_deleting() {
        let mut l = line("ac", 1);
        assert!(l.insert(b'b'));
        assert_eq!(show(&l), ("abc".into(), 2));
        l.backspace();
        assert_eq!(show(&l), ("ac".into(), 1));
        l.delete();
        assert_eq!(show(&l), ("a".into(), 1));
        l.delete();
        l.home();
        l.backspace();
        assert_eq!(show(&l), ("a".into(), 0));
        let mut full = line(&"x".repeat(MAX_LINE), MAX_LINE);
        assert!(!full.insert(b'y'));
    }

    #[test]
    fn transpose_steps_except_at_the_last_byte() {
        let mut l = line("abcd", 1);
        l.transpose();
        assert_eq!(show(&l), ("bacd".into(), 2));
        let mut last = line("abcd", 3);
        last.transpose();
        assert_eq!(show(&last), ("abdc".into(), 3));
        let mut edge = line("ab", 0);
        edge.transpose();
        assert_eq!(show(&edge), ("ab".into(), 0));
    }

    #[test]
    fn motion() {
        let mut l = line("set  my_key:1 v", 15);
        l.word_left();
        assert_eq!(l.cursor(), 14);
        l.word_left();
        assert_eq!(l.cursor(), 12, "`:` ends a word");
        l.word_left();
        assert_eq!(l.cursor(), 5, "`_` does not");
        l.word_right();
        assert_eq!(l.cursor(), 12);
        l.end();
        l.right();
        assert_eq!(l.cursor(), 15);
        l.home();
        l.left();
        assert_eq!(l.cursor(), 0);
    }

    #[test]
    fn kills() {
        let mut l = line("set k:1 v", 7);
        l.delete_prev_word();
        assert_eq!(show(&l), ("set  v".into(), 4), "space-separated: `k:1` goes whole");
        l.delete_prev_word();
        assert_eq!(show(&l), (" v".into(), 0));
        let mut k = line("abcdef", 2);
        k.kill_to_end();
        assert_eq!(show(&k), ("ab".into(), 2));
        k.kill_line();
        assert_eq!(show(&k), (String::new(), 0));
    }
}
