//! Which parts of an argument tree the words typed so far account for.
//!
//! The tree in [`Arg`] is immutable and shared; a [`Mark`] tree of the same
//! shape records the match, so one reference serves every keystroke.
//!
//! The rules are redis-cli's, observed through `--test_hint`: optional
//! arguments that sit next to each other match in any order; a repeating
//! argument takes as many repetitions as fit; integer and double arguments
//! refuse words that do not start with a number; a word that matches nothing
//! the command still needs means no hint at all.

use super::model::{Arg, Repeat, Shape, Value};

/// Match state for one [`Arg`], with one child per child argument.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mark {
    /// Words (token included) taken by the latest repetition.
    pub(crate) words: usize,
    pub(crate) token: bool,
    pub(crate) value: bool,
    /// Nothing more of this argument is expected.
    pub(crate) complete: bool,
    pub(crate) children: Vec<Mark>,
}

impl Mark {
    /// Unmatched marks for `args`.
    pub(crate) fn fresh(args: &[Arg]) -> Vec<Mark> {
        args.iter()
            .map(|a| Mark { children: Mark::fresh(children(a)), ..Mark::default() })
            .collect()
    }

    fn clear(&mut self) {
        self.words = 0;
        self.token = false;
        self.value = false;
        self.complete = false;
        self.children.iter_mut().for_each(Mark::clear);
    }
}

pub(crate) fn children(arg: &Arg) -> &[Arg] {
    match &arg.shape {
        Shape::OneOf(c) | Shape::Block(c) => c,
        Shape::Value(_) | Shape::Token => &[],
    }
}

/// Match `words` against the argument list; how many words it accounts for.
/// Stops at the first required argument the next word does not fit (0).
pub(crate) fn match_list(words: &[Vec<u8>], args: &[Arg], marks: &mut [Mark]) -> usize {
    let (mut used, mut i) = (0, 0);
    while used < words.len() && i < args.len() {
        let taken = if args[i].optional {
            let end = args[i..].iter().position(|a| !a.optional).map_or(args.len(), |n| i + n);
            let taken = match_optionals(&words[used..], &args[i..end], &mut marks[i..end]);
            i = end;
            taken
        } else {
            let taken = match_arg(&words[used..], &args[i], &mut marks[i]);
            if taken == 0 {
                return 0;
            }
            i += 1;
            taken
        };
        used += taken;
    }
    used
}

/// A run of optional arguments, in any order, each at most once.
fn match_optionals(words: &[Vec<u8>], args: &[Arg], marks: &mut [Mark]) -> usize {
    let mut used = 0;
    let mut previous: Option<usize> = None;
    while used < words.len() {
        let Some((which, taken)) = (0..args.len()).find_map(|k| {
            if marks[k].words != 0 {
                return None;
            }
            let taken = match_arg(&words[used..], &args[k], &mut marks[k]);
            (taken != 0).then_some((k, taken))
        }) else {
            break;
        };
        // Moving on to another optional argument closes the one before it.
        if let Some(p) = previous {
            marks[p].complete = true;
        }
        previous = Some(which);
        used += taken;
    }
    used
}

/// One argument, repeated while it can be.
fn match_arg(words: &[Vec<u8>], arg: &Arg, mark: &mut Mark) -> usize {
    let first = match_once(words, arg, mark);
    if arg.repeat == Repeat::Once {
        return first;
    }
    let mut used = first;
    while mark.complete && used < words.len() {
        mark.clear();
        let rest = &words[used..];
        used += if arg.token.is_some() && arg.repeat == Repeat::Many {
            let taken = match_value(rest, arg, mark);
            mark.token = mark.words != 0;
            taken
        } else {
            match_once(rest, arg, mark)
        };
    }
    // Another repetition can always follow.
    mark.complete = false;
    used
}

/// The token, if the argument has one, then its value.
fn match_once(words: &[Vec<u8>], arg: &Arg, mark: &mut Mark) -> usize {
    let mut rest = words;
    if let Some(token) = &arg.token {
        if !words[0].eq_ignore_ascii_case(token) {
            return 0;
        }
        mark.token = true;
        mark.words = 1;
        if arg.shape == Shape::Token {
            mark.complete = true;
            return 1;
        }
        if words.len() == 1 {
            return 1;
        }
        rest = &words[1..];
    }
    if match_value(rest, arg, mark) == 0 {
        return 0;
    }
    mark.words
}

/// The part after the token; adds to `mark.words` and returns the total.
fn match_value(words: &[Vec<u8>], arg: &Arg, mark: &mut Mark) -> usize {
    match &arg.shape {
        Shape::Block(list) => {
            mark.words += match_list(words, list, &mut mark.children);
            mark.complete = mark.children.iter().all(|c| c.complete);
        }
        Shape::OneOf(list) => {
            let hit = list
                .iter()
                .zip(mark.children.iter_mut())
                .find_map(|(a, m)| (match_arg(words, a, m) != 0).then_some((m.words, m.complete)));
            if let Some((taken, complete)) = hit {
                mark.words += taken;
                mark.complete = complete;
            }
        }
        Shape::Value(kind) => {
            if fits(&words[0], *kind) {
                mark.words += 1;
                mark.value = true;
                mark.complete = true;
            } else {
                mark.words = 0;
                mark.value = false;
            }
        }
        Shape::Token => {
            mark.words += 1;
            mark.value = true;
            mark.complete = true;
        }
    }
    mark.words
}

/// Whether `word` starts with a number of `kind`, after blanks.
fn fits(word: &[u8], kind: Value) -> bool {
    let start =
        word.iter().position(|b| !b.is_ascii_whitespace() && *b != 0x0b).unwrap_or(word.len());
    let body = &word[start..];
    let unsigned = body.strip_prefix(b"-").or_else(|| body.strip_prefix(b"+")).unwrap_or(body);
    match kind {
        Value::Any => true,
        Value::Integer => unsigned.first().is_some_and(u8::is_ascii_digit),
        Value::Double => starts_a_double(unsigned),
    }
}

/// A significand (`12`, `.5`) or `inf`, `infinity`, `nan`, any case.
fn starts_a_double(s: &[u8]) -> bool {
    let named = s.len() >= 3
        && (s[..3].eq_ignore_ascii_case(b"inf") || s[..3].eq_ignore_ascii_case(b"nan"));
    let s = s.strip_prefix(b".").unwrap_or(s);
    named || s.first().is_some_and(u8::is_ascii_digit)
}
