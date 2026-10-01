//! Stream command replies, byte for byte, as valkey 9.1.2 answered them.
//!
//! Each section of `data/stream_parity.txt` is a command sequence sent to a
//! running valkey, one `command<TAB>reply` line per command, the reply
//! escaped; `?` marks a reply that carries a clock and is not compared, a
//! reply after `~` is compared with the idle column of `XPENDING`'s rows
//! blanked, and `#` lines are comments. Every sequence is also replayed from the records
//! its commands wrote, and the copy must hold the same streams, groups and
//! pending entries.

#![cfg(feature = "streams-geo")]
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;

use kevy_resp::Argv;
use kevy_store::Store;
use kevy_verbs::{Effect, exec};

const CASES: &str = include_str!("data/stream_parity.txt");

struct Step<'a> {
    cmd: &'a str,
    want: Option<String>,
    /// Compare with the idle column blanked.
    idle_blank: bool,
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            other => panic!("bad escape {other:?} in {s}"),
        }
    }
    out
}

fn sections() -> Vec<(&'static str, Vec<Step<'static>>)> {
    let mut all: Vec<(&str, Vec<Step>)> = Vec::new();
    for line in CASES.lines() {
        if let Some(title) = line.strip_prefix("== ") {
            all.push((title, Vec::new()));
        } else if !line.starts_with('#') {
            let (cmd, reply) = line.split_once('\t').expect("command<TAB>reply");
            let (reply, idle_blank) = match reply.strip_prefix('~') {
                Some(r) => (r, true),
                None => (reply, false),
            };
            let want = (reply != "?").then(|| unescape(reply));
            all.last_mut().expect("a section first").1.push(Step { cmd, want, idle_blank });
        }
    }
    all
}

/// `XPENDING`'s extended rows with the idle column blanked: it is when the
/// state was read, not what it is.
fn blank_idle(reply: &str) -> String {
    let mut t: Vec<&str> = reply.split("\r\n").collect();
    for i in 0..t.len() {
        if t[i] == "*4" && t.get(i + 5).is_some_and(|x| x.starts_with(':')) {
            t[i + 5] = ":~";
        }
    }
    t.join("\r\n")
}

fn argv(cmd: &str) -> Argv {
    Argv::from(cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>())
}

fn run(store: &mut Store, cmd: &str) -> (Option<Effect>, String) {
    let a = argv(cmd);
    let mut buf = [0u8; 32];
    let verb = kevy_verbs::args::upper_verb(&a[0], &mut buf).to_vec();
    let mut out = Vec::new();
    let e = exec(store, &verb, &a, &mut out);
    (e, String::from_utf8_lossy(&out).into_owned())
}

/// The frames a recording caller writes for `cmd`, built right after it.
fn records(store: &Store, cmd: &str, e: Option<Effect>) -> Vec<Argv> {
    match e {
        Some(Effect::Write) => vec![argv(cmd)],
        Some(Effect::Record(f)) => vec![Argv::from(f)],
        Some(e) => kevy_verbs::aof::deferred_frames(store, &argv(cmd), &e),
        None => Vec::new(),
    }
}

fn replay(store: &mut Store, frame: &Argv) {
    let mut out = Vec::new();
    if !kevy_verbs::aof::apply_internal(store, frame, &mut out) {
        let mut buf = [0u8; 32];
        let verb = kevy_verbs::args::upper_verb(&frame[0], &mut buf).to_vec();
        exec(store, &verb, frame, &mut out);
    }
}

/// Everything a stream holds that a replay has to bring back.
fn dump(store: &mut Store, keys: &BTreeSet<Vec<u8>>) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        let Ok(Some(s)) = store.stream_view(key) else { continue };
        out.push(format!(
            "{} len={} last={:?} added={} maxdel={:?} nodes={} entries={:?}",
            String::from_utf8_lossy(key),
            s.length(),
            s.last_id(),
            s.entries_added(),
            s.max_deleted_id(),
            s.node_count(),
            s.range(kevy_store::StreamId::MIN, kevy_store::StreamId::MAX, None),
        ));
        for (name, g) in s.groups() {
            out.push(format!(
                "  group {} last={:?} read={:?}",
                String::from_utf8_lossy(name),
                g.last_delivered_id(),
                g.entries_read(),
            ));
            for (id, p) in g.pending_range(..) {
                out.push(format!(
                    "    {id:?} {} count={} at={}",
                    String::from_utf8_lossy(p.consumer.as_slice()),
                    p.delivery_count,
                    p.delivery_time_ms,
                ));
            }
            let mut consumers: Vec<String> = g
                .consumers()
                .map(|(n, c)| {
                    format!(
                        "    consumer {} pending={}",
                        String::from_utf8_lossy(n),
                        c.pending_count()
                    )
                })
                .collect();
            consumers.sort();
            out.extend(consumers);
        }
    }
    out
}

#[test]
fn every_reply_is_valkeys() {
    let mut compared = 0;
    for (title, steps) in sections() {
        let mut s = Store::new();
        for step in steps {
            if step.cmd == "FLUSHALL" {
                s = Store::new();
                continue;
            }
            let (_, mut got) = run(&mut s, step.cmd);
            if step.idle_blank {
                got = blank_idle(&got);
            }
            if let Some(want) = step.want {
                assert_eq!(got, want, "{title}: {}", step.cmd);
                compared += 1;
            }
        }
    }
    // an empty or unread fixture must not pass
    assert!(compared > 1000, "only {compared} replies compared");
}

#[test]
fn every_sequence_replays_from_its_records() {
    let mut checked = 0;
    for (title, steps) in sections() {
        let (mut live, mut copy) = (Store::new(), Store::new());
        let mut keys = BTreeSet::new();
        let mut settle = |live: &mut Store, copy: &mut Store, keys: &mut BTreeSet<Vec<u8>>| {
            assert_eq!(dump(copy, keys), dump(live, keys), "{title}");
            checked += keys.len();
            keys.clear();
        };
        for step in steps {
            if step.cmd == "FLUSHALL" {
                settle(&mut live, &mut copy, &mut keys);
                (live, copy) = (Store::new(), Store::new());
                continue;
            }
            let a = argv(step.cmd);
            keys.extend(a.iter().skip(1).map(<[u8]>::to_vec));
            let (e, _) = run(&mut live, step.cmd);
            for frame in records(&live, step.cmd, e) {
                replay(&mut copy, &frame);
            }
        }
        settle(&mut live, &mut copy, &mut keys);
    }
    assert!(checked > 100, "only {checked} keys compared");
}
