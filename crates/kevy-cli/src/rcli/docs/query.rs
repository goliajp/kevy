//! The questions the REPL asks of the reference: which command a line
//! names, what its hint is, and what Tab offers.

use super::marks::{Mark, match_list};
use super::model::{Docs, Entry, Params};
use super::render::remaining;
use crate::rcli::splitargs::split_args;

impl Docs {
    /// The entry whose words are the longest prefix of `words`.
    pub(crate) fn lookup(&self, words: &[Vec<u8>]) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|e| {
                e.words.len() <= words.len()
                    && e.words.iter().zip(words).all(|(a, b)| a.eq_ignore_ascii_case(b))
            })
            .fold(None, |best: Option<&Entry>, e| match best {
                Some(b) if b.words.len() >= e.words.len() => Some(b),
                _ => Some(e),
            })
    }

    /// The hint for a partly typed line, without the space the editor puts
    /// before it. The last word counts only once a blank follows it. `None`:
    /// the line names no command; empty: it does, and nothing is left to show.
    pub(crate) fn hint(&self, line: &[u8]) -> Option<Vec<u8>> {
        let words = split_args(line)?;
        let typed = match line.last() {
            Some(b) if is_blank(*b) => words.len(),
            _ => words.len().checked_sub(1)?,
        };
        let entry = self.lookup(&words[..typed])?;
        let after = &words[entry.words.len()..typed];
        Some(match &entry.params {
            Params::Args(args) => {
                let mut marks = Mark::fresh(args);
                if match_list(after, args, &mut marks) == after.len() {
                    remaining(args, &mut marks)
                } else {
                    Vec::new()
                }
            }
            Params::Syntax(text) if after.is_empty() => text.clone(),
            Params::Syntax(_) | Params::Unknown => Vec::new(),
        })
    }

    /// Whole-line candidates for Tab: command names the line is a prefix of,
    /// and after `help ` also `@group` topics, in name order.
    pub(crate) fn completions(&self, line: &[u8]) -> Vec<Vec<u8>> {
        let after_help = line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"help ");
        let start =
            if after_help { 5 + line[5..].iter().take_while(|b| is_blank(**b)).count() } else { 0 };
        let (kept, typed) = line.split_at(start);
        let prefixed = |name: &[u8]| {
            name.len() >= typed.len() && name[..typed.len()].eq_ignore_ascii_case(typed)
        };
        let commands = self.entries.iter().map(|e| e.full.clone());
        let groups =
            self.groups.iter().filter(|_| after_help).map(|g| [b"@", g.as_slice()].concat());
        let mut names: Vec<Vec<u8>> = commands.chain(groups).filter(|n| prefixed(n)).collect();
        names.sort();
        names.into_iter().map(|n| [kept, n.as_slice()].concat()).collect()
    }
}

impl Entry {
    /// The syntax `help` prints after the name.
    pub(crate) fn params_text(&self) -> Vec<u8> {
        match &self.params {
            Params::Args(args) => remaining(args, &mut Mark::fresh(args)),
            Params::Syntax(text) => text.clone(),
            Params::Unknown => Vec::new(),
        }
    }
}

/// C's `isspace`.
fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
