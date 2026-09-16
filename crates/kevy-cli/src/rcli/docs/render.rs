//! The argument syntax still to type, as text: `key value [NX|XX] [GET]`.
//!
//! Fresh marks render a command's whole syntax (what `help` prints); marks
//! from a match render only what the typed words leave open (the hint).

use super::marks::{Mark, children};
use super::model::{Arg, Repeat, Shape};

/// The syntax `args` still expect, given `marks`.
pub(crate) fn remaining(args: &[Arg], marks: &mut [Mark]) -> Vec<u8> {
    let mut out = Vec::new();
    list(&mut out, args, marks, b" ");
    out
}

/// A list of arguments joined by `sep`. A run of optional arguments shows
/// the one being typed first, and disappears once a required argument after
/// it has been reached.
fn list(out: &mut Vec<u8>, args: &[Arg], marks: &mut [Mark], sep: &[u8]) {
    let mut joined = out.len();
    let last = args.len().saturating_sub(1);
    let mut i = 0;
    while i < args.len() {
        if !args[i].optional {
            one(out, &args[i], &mut marks[i]);
            separate(out, &mut joined, sep, i == last);
            i += 1;
            continue;
        }
        let end = args[i..].iter().position(|a| !a.optional).map_or(args.len(), |n| i + n);
        let mut in_progress = None;
        for k in i..end {
            if marks[k].words != 0 && !marks[k].complete {
                one(out, &args[k], &mut marks[k]);
                separate(out, &mut joined, sep, i == last);
                in_progress = Some(k);
            }
        }
        if end == args.len() || marks[end].words == 0 {
            for k in i..end {
                if in_progress != Some(k) {
                    one(out, &args[k], &mut marks[k]);
                    separate(out, &mut joined, sep, k == last);
                }
            }
        }
        i = end;
    }
}

/// `sep` after text an argument added, unless it is the list's last.
fn separate(out: &mut Vec<u8>, joined: &mut usize, sep: &[u8], is_last: bool) {
    if out.len() > *joined && !is_last {
        out.extend_from_slice(sep);
        *joined = out.len();
    }
}

/// One argument: bracketed when optional and untouched, its token unless
/// typed, its value or children, then its repetition.
fn one(out: &mut Vec<u8>, arg: &Arg, mark: &mut Mark) {
    if mark.complete {
        return;
    }
    let bracketed = arg.optional && mark.words == 0;
    if bracketed {
        out.push(b'[');
    }
    if let Some(token) = arg.token.as_ref().filter(|_| !mark.token) {
        word(out, token);
        if arg.shape != Shape::Token {
            out.push(b' ');
        }
    }
    match &arg.shape {
        Shape::OneOf(choices) if mark.words != 0 => {
            for (choice, m) in choices.iter().zip(mark.children.iter_mut()) {
                if m.words != 0 {
                    one(out, choice, m);
                }
            }
        }
        Shape::OneOf(choices) => list(out, choices, &mut mark.children, b"|"),
        Shape::Block(parts) => list(out, parts, &mut mark.children, b" "),
        Shape::Token => {}
        Shape::Value(_) if !mark.value => word(out, &arg.display),
        Shape::Value(_) => {}
    }
    repetition(out, arg, mark);
    if bracketed {
        out.push(b']');
    }
}

/// ` [TOKEN value ...]` for an argument that repeats: the unit, untouched.
fn repetition(out: &mut Vec<u8>, arg: &Arg, mark: &mut Mark) {
    if arg.repeat == Repeat::Once {
        return;
    }
    *mark = Mark { children: Mark::fresh(children(arg)), ..Mark::default() };
    if !out.is_empty() {
        out.push(b' ');
    }
    out.push(b'[');
    if let (Repeat::ManyWithToken, Some(token)) = (arg.repeat, &arg.token) {
        word(out, token);
        if arg.shape != Shape::Token {
            out.push(b' ');
        }
    }
    match &arg.shape {
        Shape::OneOf(choices) => list(out, choices, &mut mark.children, b"|"),
        Shape::Block(parts) => list(out, parts, &mut mark.children, b" "),
        Shape::Token => {}
        Shape::Value(_) => word(out, &arg.display),
    }
    out.extend_from_slice(b" ...]");
}

/// A name as written, or `""` when it is empty.
fn word(out: &mut Vec<u8>, name: &[u8]) {
    if name.is_empty() {
        out.extend_from_slice(b"\"\"");
    } else {
        out.extend_from_slice(name);
    }
}
