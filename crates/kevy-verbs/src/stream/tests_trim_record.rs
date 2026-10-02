//! An approximate trim with `DELREF` or `ACKED` is recorded as an exact
//! one; replaying the record must leave a stream exactly as the command
//! did, entries and pending lists alike.

use kevy_resp::Argv;
use kevy_store::Store;

use crate::{Effect, exec};

fn run(s: &mut Store, words: &[&str]) -> (Vec<u8>, Option<Effect>) {
    let argv = Argv::from(words.iter().map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>());
    let mut out = Vec::new();
    let e = exec(s, &words[0].to_ascii_uppercase().into_bytes(), &argv, &mut out);
    (out, e)
}

/// 250 entries; a group reads 150 and acknowledges every other one of
/// them, so what ACKED may remove is scattered.
fn built() -> Store {
    let mut s = Store::new();
    for i in 1..=250 {
        run(&mut s, &["XADD", "s", &format!("{i}-0"), "f", "v"]);
    }
    run(&mut s, &["XGROUP", "CREATE", "s", "g", "0"]);
    run(&mut s, &["XREADGROUP", "GROUP", "g", "c", "COUNT", "150", "STREAMS", "s", ">"]);
    let acks: Vec<String> = (1..=150).step_by(2).map(|i| format!("{i}-0")).collect();
    let mut ack = vec!["XACK", "s", "g"];
    ack.extend(acks.iter().map(String::as_str));
    run(&mut s, &ack);
    s
}

/// The entries, and the pending list's size and bounds (its extended form
/// carries idle times, which two stores built apart never share).
fn state(s: &mut Store) -> (Vec<u8>, Vec<u8>) {
    (run(s, &["XRANGE", "s", "-", "+"]).0, run(s, &["XPENDING", "s", "g"]).0)
}

#[test]
fn a_replayed_record_leaves_the_stream_as_the_command_did() {
    let cases: &[&[&str]] = &[
        &["XTRIM", "s", "MAXLEN", "~", "0", "ACKED", "LIMIT", "150"],
        &["XTRIM", "s", "MAXLEN", "~", "0", "ACKED", "LIMIT", "100"],
        &["XTRIM", "s", "MAXLEN", "~", "0", "ACKED"],
        &["XTRIM", "s", "MINID", "~", "120-0", "ACKED"],
        &["XTRIM", "s", "MAXLEN", "~", "60", "DELREF"],
        &["XTRIM", "s", "MAXLEN", "~", "100", "DELREF", "LIMIT", "100"],
        &["XADD", "s", "MAXLEN", "~", "0", "ACKED", "LIMIT", "150", "*", "f", "v"],
        &["XADD", "s", "MAXLEN", "~", "30", "DELREF", "251-0", "f", "v"],
    ];
    for cmd in cases {
        let mut live = built();
        let (_, effect) = run(&mut live, cmd);
        let Some(Effect::Record(frame)) = effect else {
            panic!("{cmd:?} is recorded as the exact trim it made, got {effect:?}");
        };
        let words: Vec<String> =
            frame.iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect();
        let mut replay = built();
        run(&mut replay, &words.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(state(&mut replay), state(&mut live), "{cmd:?} recorded as {words:?}");
    }
}
