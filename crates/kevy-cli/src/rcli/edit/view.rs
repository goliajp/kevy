//! Drawing the line being edited on a terminal.
//!
//! One frame is the prompt, the line (or `*`s when masked, or the search
//! match with its term in bold), and an optional grey hint, wrapped at the
//! terminal width. A redraw returns the cursor to the frame's first row,
//! clears to the end of the screen and writes the frame again, so it is
//! correct whatever the previous frame looked like — including one that
//! wrapped onto more rows than this one.

/// What the frame shows after the prompt.
#[derive(Debug)]
pub(crate) enum Body<'a> {
    Text(&'a [u8]),
    /// Password entry: one `*` per byte.
    Masked(usize),
    /// Incremental search: `line` with `line[start..start+len]` in bold.
    Match {
        line: &'a [u8],
        start: usize,
        len: usize,
    },
}

impl Body<'_> {
    fn width(&self) -> usize {
        match self {
            Body::Text(t) => t.len(),
            Body::Masked(n) => *n,
            Body::Match { line, .. } => line.len(),
        }
    }
}

/// One frame to draw.
#[derive(Debug)]
pub(crate) struct Frame<'a> {
    pub(crate) prompt: &'a [u8],
    pub(crate) body: Body<'a>,
    /// Byte offset of the cursor within the body.
    pub(crate) cursor: usize,
    pub(crate) hint: Option<&'a [u8]>,
}

/// Where the last frame left the terminal cursor.
#[derive(Debug, Default)]
pub(crate) struct View {
    /// Rows from the frame's first row down to the cursor.
    cursor_row: usize,
}

impl View {
    /// The bytes that replace the previous frame with `f` on a terminal
    /// `cols` wide.
    pub(crate) fn draw(&mut self, f: &Frame<'_>, cols: usize) -> Vec<u8> {
        let cols = cols.max(1);
        let mut out = Vec::with_capacity(f.prompt.len() + f.body.width() + 32);
        if self.cursor_row > 0 {
            out.extend_from_slice(format!("\x1b[{}A", self.cursor_row).as_bytes());
        }
        out.extend_from_slice(b"\r\x1b[0J");
        out.extend_from_slice(f.prompt);
        body(&mut out, &f.body);
        let mut end = f.prompt.len() + f.body.width();
        if let Some(h) = f.hint {
            out.extend_from_slice(b"\x1b[0;90;49m");
            out.extend_from_slice(h);
            out.extend_from_slice(b"\x1b[0m");
            end += h.len();
        }
        let cursor = f.prompt.len() + f.cursor;
        // A frame that fills its last row exactly leaves the terminal's
        // cursor pending at that row's end; step onto a fresh row so that
        // the arithmetic below and the next draw start from a real column.
        let end_row = if end > 0 && end.is_multiple_of(cols) {
            out.extend_from_slice(b"\r\n");
            end / cols
        } else {
            end / cols
        };
        let row = cursor / cols;
        if end_row > row {
            out.extend_from_slice(format!("\x1b[{}A", end_row - row).as_bytes());
        }
        out.push(b'\r');
        if !cursor.is_multiple_of(cols) {
            out.extend_from_slice(format!("\x1b[{}C", cursor % cols).as_bytes());
        }
        self.cursor_row = row;
        out
    }

    /// Forget the frame: the next draw starts on the current row (after the
    /// line was accepted and the cursor moved below it).
    pub(crate) fn reset(&mut self) {
        self.cursor_row = 0;
    }
}

fn body(out: &mut Vec<u8>, b: &Body<'_>) {
    match b {
        Body::Text(t) => out.extend_from_slice(t),
        Body::Masked(n) => out.resize(out.len() + n, b'*'),
        Body::Match { line, start, len } => {
            out.extend_from_slice(&line[..*start]);
            out.extend_from_slice(b"\x1b[1m");
            out.extend_from_slice(&line[*start..start + len]);
            out.extend_from_slice(b"\x1b[0m");
            out.extend_from_slice(&line[start + len..]);
        }
    }
}

#[cfg(test)]
pub(crate) mod screen {
    //! A terminal just capable enough to check what a user would see:
    //! printable bytes, CR, LF (as CR+LF, the way a terminal in cooked
    //! output mode shows it), CSI A/B/C/D, `0J`, `0K`, `H`, `2J`. SGR is
    //! dropped. The cursor wraps to the next row after the last column.

    pub(crate) struct Screen {
        pub(crate) rows: Vec<Vec<u8>>,
        pub(crate) row: usize,
        pub(crate) col: usize,
        cols: usize,
        pending_wrap: bool,
    }

    impl Screen {
        pub(crate) fn new(cols: usize) -> Screen {
            Screen { rows: vec![Vec::new()], row: 0, col: 0, cols, pending_wrap: false }
        }

        pub(crate) fn text(&self) -> Vec<String> {
            let mut lines: Vec<String> = self
                .rows
                .iter()
                .map(|r| String::from_utf8_lossy(r).trim_end().to_string())
                .collect();
            while lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
            lines
        }

        fn row_mut(&mut self) -> &mut Vec<u8> {
            while self.rows.len() <= self.row {
                self.rows.push(Vec::new());
            }
            &mut self.rows[self.row]
        }

        pub(crate) fn feed(&mut self, bytes: &[u8]) {
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'\r' => (self.col, self.pending_wrap) = (0, false),
                    b'\n' => (self.row, self.col, self.pending_wrap) = (self.row + 1, 0, false),
                    0x1b if bytes.get(i + 1) == Some(&b'[') => {
                        let start = i + 2;
                        let mut j = start;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        self.csi(&bytes[start..j], bytes.get(j).copied().unwrap_or(0));
                        i = j;
                    }
                    b => self.put(b),
                }
                i += 1;
            }
        }

        fn put(&mut self, b: u8) {
            if self.pending_wrap {
                (self.row, self.col, self.pending_wrap) = (self.row + 1, 0, false);
            }
            let col = self.col;
            let row = self.row_mut();
            if row.len() <= col {
                row.resize(col + 1, b' ');
            }
            row[col] = b;
            if self.col + 1 == self.cols {
                self.pending_wrap = true;
            } else {
                self.col += 1;
            }
        }

        fn csi(&mut self, params: &[u8], fin: u8) {
            let n: usize =
                std::str::from_utf8(params).ok().and_then(|p| p.parse().ok()).unwrap_or(1);
            self.pending_wrap = false;
            match fin {
                b'A' => self.row = self.row.saturating_sub(n),
                b'B' => self.row += n,
                b'C' => self.col = (self.col + n).min(self.cols - 1),
                b'D' => self.col = self.col.saturating_sub(n),
                b'J' => {
                    let (row, col) = (self.row, self.col);
                    self.row_mut().truncate(col);
                    self.rows.truncate(row + 1);
                }
                b'K' => {
                    let col = self.col;
                    self.row_mut().truncate(col);
                }
                b'H' => (self.row, self.col) = (0, 0),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::screen::Screen;
    use super::{Body, Frame, View};

    fn frame<'a>(prompt: &'a str, text: &'a str, cursor: usize) -> Frame<'a> {
        Frame { prompt: prompt.as_bytes(), body: Body::Text(text.as_bytes()), cursor, hint: None }
    }

    #[test]
    fn a_line_and_its_cursor() {
        let (mut s, mut v) = (Screen::new(80), View::default());
        s.feed(&v.draw(&frame("> ", "GET key", 3), 80));
        assert_eq!(s.text(), ["> GET key"]);
        assert_eq!((s.row, s.col), (0, 5));
    }

    #[test]
    fn wrapping_and_shrinking_back() {
        let (mut s, mut v) = (Screen::new(10), View::default());
        s.feed(&v.draw(&frame("> ", "abcdefghijklmnop", 16), 10));
        assert_eq!(s.text(), ["> abcdefgh", "ijklmnop"]);
        assert_eq!((s.row, s.col), (1, 8));
        s.feed(&v.draw(&frame("> ", "abc", 1), 10));
        assert_eq!(s.text(), ["> abc"], "the second row is cleared");
        assert_eq!((s.row, s.col), (0, 3));
        // Exactly filling a row moves the cursor onto the next one.
        s.feed(&v.draw(&frame("> ", "abcdefgh", 8), 10));
        assert_eq!(s.text(), ["> abcdefgh"]);
        assert_eq!((s.row, s.col), (1, 0));
        s.feed(&v.draw(&frame("> ", "abcdefgh", 0), 10));
        assert_eq!((s.row, s.col), (0, 2));
    }

    #[test]
    fn mask_match_and_hint() {
        let (mut s, mut v) = (Screen::new(80), View::default());
        let masked = Frame { prompt: b"pw: ", body: Body::Masked(3), cursor: 3, hint: None };
        s.feed(&v.draw(&masked, 80));
        assert_eq!(s.text(), ["pw: ***"]);
        let found = Frame {
            prompt: b"(reverse-i-search): ",
            body: Body::Match { line: b"SET k v", start: 4, len: 1 },
            cursor: 0,
            hint: None,
        };
        s.feed(&v.draw(&found, 80));
        assert_eq!(s.text(), ["(reverse-i-search): SET k v"]);
        let hinted =
            Frame { prompt: b"> ", body: Body::Text(b"GET "), cursor: 4, hint: Some(b"key") };
        s.feed(&v.draw(&hinted, 80));
        assert_eq!(s.text(), ["> GET key"]);
        assert_eq!(s.col, 6, "the hint does not move the cursor");
        v.reset();
        assert_eq!(v.cursor_row, 0);
    }
}
