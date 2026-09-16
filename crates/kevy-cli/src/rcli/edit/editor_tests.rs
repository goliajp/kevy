//! The editor driven by scripted keystrokes, checked on a model screen.

use super::super::history::History;
use super::super::view::screen::Screen;
use super::{Assist, Outcome, no_assist, read_line};

fn history(lines: &[&str]) -> History {
    let mut h = History::default();
    for l in lines {
        h.add(l.as_bytes(), false);
    }
    h
}

/// Type `keys` at `prompt`; the outcome and the final screen.
fn type_keys(keys: &[u8], h: &History, assist: &Assist<'_>, mask: bool) -> (Outcome, Vec<String>) {
    let mut input = keys;
    let mut out = Vec::new();
    let outcome = read_line(&mut input, &mut out, b"> ", h, assist, 40, mask)
        .expect("in-memory I/O does not fail");
    let mut screen = Screen::new(40);
    screen.feed(&out);
    (outcome, screen.text())
}

fn line(s: &str) -> Outcome {
    Outcome::Line(s.as_bytes().to_vec())
}

#[test]
fn typing_editing_and_the_ends_of_a_line() {
    let none = no_assist();
    let h = History::default();
    assert_eq!(
        type_keys(b"GET k\r", &h, &none, false),
        (line("GET k"), vec!["> GET k".to_string()])
    );
    // Home, "X ", End, "!", back two to before T, swap E and T.
    assert_eq!(type_keys(b"SET\x01X \x05!\x02\x02\x14\r", &h, &none, false).0, line("X STE!"));
    assert_eq!(type_keys(b"abc def\x17\x17xy\x15z\r", &h, &none, false).0, line("z"));
    assert_eq!(type_keys(b"hello\x1bb\x0bbye\r", &h, &none, false).0, line("bye"));
    assert_eq!(type_keys(b"ab\x1b[D\x1b[3~\x04c\r", &h, &none, false).0, line("ac"));
    assert_eq!(type_keys(b"x\n\r", &h, &none, false).0, line("x"), "a line feed does nothing");
    assert_eq!(type_keys(b"half", &h, &none, false).0, Outcome::Eof, "input ends mid-line");
    assert_eq!(type_keys(b"\x04", &h, &none, false).0, Outcome::Eof);
    assert_eq!(type_keys(b"abc\x03", &h, &none, false).0, Outcome::Interrupted);
    let (_, screen) = type_keys(b"one\x0ctwo\r", &h, &none, false);
    assert_eq!(screen, ["> onetwo"], "Ctrl-L clears and redraws the line on top");
    assert_eq!(type_keys(b"pw\r", &h, &none, true), (line("pw"), vec!["> **".to_string()]));
}

#[test]
fn history_recall_keeps_edits_while_moving() {
    let none = no_assist();
    let h = history(&["GET a", "GET b"]);
    assert_eq!(type_keys(b"\x10\x10\r", &h, &none, false).0, line("GET a"));
    assert_eq!(
        type_keys(b"\x1b[A\x1b[A\x1b[A\x1b[B\r", &h, &none, false).0,
        line("GET b"),
        "no older than the oldest"
    );
    assert_eq!(type_keys(b"new\x10X\x0e\x10\r", &h, &none, false).0, line("GET bX"));
    assert_eq!(type_keys(b"new\x10\x0e\r", &h, &none, false).0, line("new"));
    assert_eq!(type_keys(b"\x0e\r", &h, &none, false).0, line(""));
    assert_eq!(type_keys(b"\x10\r", &History::default(), &none, false).0, line(""));
}

#[test]
fn incremental_search() {
    let none = no_assist();
    let h = history(&["SET a 1", "GET a", "SET b 2"]);
    let (outcome, screen) = type_keys(b"\x12SET\x12\r", &h, &none, false);
    assert_eq!(outcome, line("SET a 1"));
    assert_eq!(screen, ["> SET a 1"]);
    assert_eq!(
        type_keys(b"\x12GET\tX\r", &h, &none, false).0,
        line("GET aX"),
        "Tab takes the match and keeps editing"
    );
    assert_eq!(
        type_keys(b"\x12SET\x07more\r", &h, &none, false).0,
        line("more"),
        "Ctrl-G drops it"
    );
    assert_eq!(type_keys(b"\x13a\x13\r", &h, &none, false).0, line("GET a"));
    assert_eq!(
        type_keys(b"\x12zzz\r", &h, &none, false).0,
        line("zzz"),
        "no match: the term is the line"
    );
    assert_eq!(type_keys(b"\x12SET\x1b[Cok\r", &h, &none, false).0, line("ok"));
}

#[test]
fn hints_and_completion() {
    let hint = |line: &[u8]| (line == b"GET ").then(|| b"key".to_vec());
    let complete = |line: &[u8]| {
        if line.eq_ignore_ascii_case(b"cl") {
            vec![b"CLIENT".to_vec(), b"CLUSTER".to_vec()]
        } else {
            Vec::new()
        }
    };
    let assist = Assist { hint: &hint, complete: &complete };
    let h = History::default();
    let mut out = Vec::new();
    let mut input: &[u8] = b"GET ";
    let _ = read_line(&mut input, &mut out, b"> ", &h, &assist, 40, false);
    let mut screen = Screen::new(40);
    screen.feed(&out);
    assert_eq!(screen.text()[0], "> GET key", "the hint shows while typing");
    assert_eq!(
        type_keys(b"GET \r", &h, &assist, false),
        (line("GET "), vec!["> GET".to_string()]),
        "and not once accepted"
    );
    assert_eq!(type_keys(b"cl\t\r", &h, &assist, false).0, line("CLIENT"));
    assert_eq!(type_keys(b"cl\t\t\r", &h, &assist, false).0, line("CLUSTER"));
    assert_eq!(
        type_keys(b"cl\t\t\t\r", &h, &assist, false).0,
        line("cl"),
        "past the last, the original"
    );
    assert_eq!(type_keys(b"cl\t\x1b!\r", &h, &assist, false).0, line("cl!"), "Esc restores");
    assert_eq!(type_keys(b"cl\tX\r", &h, &assist, false).0, line("CLIENTX"), "another key accepts");
    assert_eq!(type_keys(b"zz\t\r", &h, &assist, false).0, line("zz"));
    let long = [b"GET ".as_slice(), &[b'x'; 40]].concat();
    let mut typed = long.clone();
    typed.push(b'\r');
    assert_eq!(type_keys(&typed, &h, &assist, false).0, Outcome::Line(long));
}
