//! Reading one line from a terminal: keys in, frames out.
//!
//! The editor knows nothing about commands. What to hint and what to offer
//! on Tab come in through [`Assist`], so the same loop serves the command
//! prompt and a password prompt.

use super::history::History;
use super::keys::{Decoder, Key};
use super::line::Line;
use super::search::{self, Direction, Found};
use super::view::{Body, Frame, View};
use std::io::{self, BufRead, Write};

/// How reading a line ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Line(Vec<u8>),
    /// Ctrl-C.
    Interrupted,
    /// Ctrl-D on an empty line, or the input ended.
    Eof,
}

/// Help the editor gets from its caller.
pub(crate) struct Assist<'a> {
    /// Grey text shown after the line, if any.
    pub(crate) hint: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
    /// Whole-line replacements offered by Tab, in order.
    pub(crate) complete: &'a dyn Fn(&[u8]) -> Vec<Vec<u8>>,
}

/// No hints, no completion.
#[cfg(test)]
pub(crate) fn no_assist() -> Assist<'static> {
    Assist { hint: &|_| None, complete: &|_| Vec::new() }
}

struct Search {
    dir: Direction,
    found: Option<Found>,
}

/// Tab cycling: the candidates, and which one shows (`len` = the original).
struct Completion {
    original: Line,
    candidates: Vec<Vec<u8>>,
    shown: usize,
}

/// One line's editing state.
struct Session<'a> {
    prompt: &'a [u8],
    line: Line,
    view: View,
    cols: usize,
    mask: bool,
    history: &'a History,
    /// Recall position: 0 is the line being typed, `k` the k-th newest entry.
    recall: usize,
    /// Edits made to recalled lines during this session, by recall position.
    edits: Vec<Option<Vec<u8>>>,
    search: Option<Search>,
    completion: Option<Completion>,
    /// A search just ended by taking its match: show no hint until the next key.
    quiet_hint: bool,
}

/// Read one line from `input`, drawing on `out`.
pub(crate) fn read_line(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    prompt: &[u8],
    history: &History,
    assist: &Assist<'_>,
    cols: usize,
    mask: bool,
) -> io::Result<Outcome> {
    let mut s = Session {
        prompt,
        line: Line::default(),
        view: View::default(),
        cols,
        mask,
        history,
        recall: 0,
        edits: vec![None; history.len() + 1],
        search: None,
        completion: None,
        quiet_hint: false,
    };
    s.redraw(out, assist)?;
    let outcome = s.run(input, out, assist)?;
    out.write_all(b"\r\n")?;
    out.flush()?;
    Ok(outcome)
}

impl Session<'_> {
    fn run(
        &mut self,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
        assist: &Assist<'_>,
    ) -> io::Result<Outcome> {
        let mut decoder = Decoder::default();
        loop {
            let typed = input.fill_buf()?;
            if typed.is_empty() {
                return Ok(Outcome::Eof);
            }
            // Bytes after the key that ends the line stay buffered for the
            // next line: typed ahead, pasted, or piped.
            let (mut used, typed) = (0, typed.to_vec());
            for &b in &typed {
                used += 1;
                if self.completion.is_some() && b == 0x1b {
                    self.restore_completion();
                    self.redraw(out, assist)?;
                    continue;
                }
                let Some(key) = decoder.feed(b) else { continue };
                if let Some(done) = self.key(key, out, assist)? {
                    input.consume(used);
                    return Ok(done);
                }
            }
            input.consume(used);
        }
    }

    /// Apply one key; `Some` when it ends the line.
    fn key(
        &mut self,
        key: Key,
        out: &mut dyn Write,
        assist: &Assist<'_>,
    ) -> io::Result<Option<Outcome>> {
        if key == Key::Tab && self.search.is_none() {
            self.tab(assist);
            self.redraw(out, assist)?;
            return Ok(None);
        }
        self.completion = None;
        if self.search.is_some()
            && let Some(done) = self.search_key(key)
        {
            return self.finish(done, out, assist);
        }
        let done = self.edit(key, out)?;
        if done.is_some() {
            return self.finish(done, out, assist);
        }
        self.redraw(out, assist)?;
        Ok(None)
    }

    /// Keys that mean something different while searching. `Some(None)`:
    /// handled, keep editing; `Some(Some(_))`: the line ends; `None`: an
    /// ordinary edit to the search term.
    fn search_key(&mut self, key: Key) -> Option<Option<Outcome>> {
        match key {
            Key::Enter => {
                self.take_match();
                Some(Some(Outcome::Line(self.line.bytes().to_vec())))
            }
            Key::Tab => {
                self.take_match();
                Some(None)
            }
            Key::Interrupt
            | Key::Cancel
            | Key::WordLeft
            | Key::WordRight
            | Key::Unbound
            | Key::HistoryPrev
            | Key::HistoryNext
            | Key::Left
            | Key::Right
            | Key::Home
            | Key::End
            | Key::Delete => {
                self.line.kill_line();
                self.search = None;
                Some(None)
            }
            Key::SearchBackward | Key::SearchForward => {
                let dir = if key == Key::SearchBackward {
                    Direction::Backward
                } else {
                    Direction::Forward
                };
                self.cycle_search(dir);
                Some(None)
            }
            _ => None,
        }
    }

    fn take_match(&mut self) {
        if let Some(Search { found: Some(f), .. }) = self.search.take()
            && let Some(line) = self.history.newest(f.back)
        {
            self.line.set(line);
        }
        self.quiet_hint = true;
    }

    fn cycle_search(&mut self, dir: Direction) {
        if let Some(search) = self.search.as_mut() {
            search.dir = dir;
            if let Some(next) = search::find(self.history, self.line.bytes(), dir, search.found) {
                search.found = Some(next);
            }
        }
    }

    /// An ordinary edit; `Some` when the key ends the line.
    fn edit(&mut self, key: Key, out: &mut dyn Write) -> io::Result<Option<Outcome>> {
        match key {
            Key::Enter => return Ok(Some(Outcome::Line(self.line.bytes().to_vec()))),
            Key::Interrupt => return Ok(Some(Outcome::Interrupted)),
            Key::DeleteOrEof if self.line.bytes().is_empty() => return Ok(Some(Outcome::Eof)),
            Key::DeleteOrEof | Key::Delete => self.line.delete(),
            Key::SearchBackward | Key::SearchForward => self.start_search(key),
            Key::HistoryPrev => self.recall(1),
            Key::HistoryNext => self.recall(-1),
            Key::ClearScreen => {
                out.write_all(b"\x1b[H\x1b[2J")?;
                self.view.reset();
            }
            other => self.motion_or_change(other),
        }
        self.refresh_search();
        Ok(None)
    }

    // LOC-WAIVER: a dispatch table — one arm per key, each a single call.
    fn motion_or_change(&mut self, key: Key) {
        let l = &mut self.line;
        match key {
            Key::Insert(b) => {
                l.insert(b);
            }
            Key::Backspace => l.backspace(),
            Key::Transpose => l.transpose(),
            Key::Left => l.left(),
            Key::Right => l.right(),
            Key::WordLeft => l.word_left(),
            Key::WordRight => l.word_right(),
            Key::Home => l.home(),
            Key::End => l.end(),
            Key::KillLine => l.kill_line(),
            Key::KillToEnd => l.kill_to_end(),
            Key::DeletePrevWord => l.delete_prev_word(),
            _ => {}
        }
    }

    fn start_search(&mut self, key: Key) {
        let dir = if key == Key::SearchBackward { Direction::Backward } else { Direction::Forward };
        self.line.kill_line();
        self.search = Some(Search { dir, found: None });
    }

    /// Re-run the search for the current term from the start.
    fn refresh_search(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.found = search::find(self.history, self.line.bytes(), search.dir, None);
        }
    }

    /// Up (`step` 1) or down (`step` -1) through history, keeping edits.
    fn recall(&mut self, step: isize) {
        if self.history.len() == 0 {
            return;
        }
        let target = self.recall as isize + step;
        if target < 0 || target as usize > self.history.len() {
            return;
        }
        self.edits[self.recall] = Some(self.line.bytes().to_vec());
        self.recall = target as usize;
        let text = match &self.edits[self.recall] {
            Some(edited) => edited.clone(),
            None => self.history.newest(self.recall - 1).unwrap_or_default().to_vec(),
        };
        self.line.set(&text);
    }

    fn tab(&mut self, assist: &Assist<'_>) {
        match self.completion.as_mut() {
            None => {
                let candidates = (assist.complete)(self.line.bytes());
                if candidates.is_empty() {
                    beep();
                    return;
                }
                self.completion =
                    Some(Completion { original: self.line.clone(), candidates, shown: 0 });
            }
            Some(c) => {
                c.shown = (c.shown + 1) % (c.candidates.len() + 1);
                if c.shown == c.candidates.len() {
                    beep();
                }
            }
        }
        self.show_completion();
    }

    fn show_completion(&mut self) {
        if let Some(c) = &self.completion {
            let text = c.candidates.get(c.shown).cloned();
            match text {
                Some(t) => self.line.set(&t),
                None => self.line = c.original.clone(),
            }
        }
    }

    fn restore_completion(&mut self) {
        if let Some(c) = self.completion.take() {
            self.line = c.original;
        }
    }

    /// Draw the line once more without its hint, then end.
    fn finish(
        &mut self,
        done: Option<Outcome>,
        out: &mut dyn Write,
        assist: &Assist<'_>,
    ) -> io::Result<Option<Outcome>> {
        if let Some(Outcome::Line(_)) = &done {
            self.line.end();
            self.quiet_hint = true;
            self.redraw(out, assist)?;
        }
        Ok(done)
    }

    fn redraw(&mut self, out: &mut dyn Write, assist: &Assist<'_>) -> io::Result<()> {
        let prompt = match &self.search {
            Some(search) => search::prompt(search.dir),
            None => self.prompt,
        };
        let matched = self
            .search
            .as_ref()
            .and_then(|s| s.found)
            .and_then(|f| self.history.newest(f.back).map(|line| (line, f.start)));
        let (body, cursor) = match matched {
            Some((line, start)) => (
                Body::Match { line, start, len: self.line.bytes().len() },
                start + self.line.cursor(),
            ),
            None if self.mask => (Body::Masked(self.line.bytes().len()), self.line.cursor()),
            None => (Body::Text(self.line.bytes()), self.line.cursor()),
        };
        let hint = self.hint_for(prompt.len(), assist);
        let frame = Frame { prompt, body, cursor, hint: hint.as_deref() };
        let bytes = self.view.draw(&frame, self.cols);
        self.quiet_hint = false;
        out.write_all(&bytes)?;
        out.flush()
    }

    /// The hint, cut to what fits on the row; none while searching, masking,
    /// or right after a search match was taken.
    fn hint_for(&self, prompt_len: usize, assist: &Assist<'_>) -> Option<Vec<u8>> {
        if self.search.is_some() || self.mask || self.quiet_hint {
            return None;
        }
        let used = prompt_len + self.line.bytes().len();
        if used >= self.cols {
            return None;
        }
        let mut hint = (assist.hint)(self.line.bytes())?;
        hint.truncate(self.cols - used);
        Some(hint)
    }
}

/// The terminal bell, on stderr so it never lands in captured output.
fn beep() {
    let _ = io::stderr().write_all(b"\x07"); // a bell nobody hears changes nothing
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;
