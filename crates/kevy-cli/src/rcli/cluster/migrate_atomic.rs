//! Atomic slot migration: the target imports each source's slot ranges as
//! one task, and the client waits for the task to finish.

use super::migrate::Progress;
use super::topology::Cluster;
use crate::rcli::send::write_out;
use std::time::{Duration, Instant};

/// How long a task may run without `--cluster-timeout`.
const DEFAULT_TIMEOUT_MS: u64 = 3_600_000;

/// One task per source, in plan order.
pub(crate) fn run(
    c: &mut Cluster,
    target: usize,
    moves: &[(usize, u16)],
    progress: Progress,
) -> bool {
    let mut k = 0;
    while k < moves.len() {
        let source = moves[k].0;
        let end = k + moves[k..].iter().take_while(|m| m.0 == source).count();
        let slots: Vec<u16> = moves[k..end].iter().map(|m| m.1).collect();
        if !one_task(c, (source, target), &slots, progress) {
            return false;
        }
        if progress == Progress::Hashes {
            write_out(&vec![b'#'; slots.len()]);
        }
        k = end;
    }
    true
}

fn one_task(
    c: &mut Cluster,
    (source, target): (usize, usize),
    slots: &[u16],
    progress: Progress,
) -> bool {
    let steps = progress == Progress::Steps;
    let text = [
        format!("Moving {} slots from ", slots.len()).as_bytes(),
        &c.nodes[source].shown(),
        b" to ",
        &c.nodes[target].shown(),
        b"\n",
    ]
    .concat();
    if steps {
        write_out(&text);
    }
    let bounds: Vec<Vec<u8>> = ranges(slots)
        .into_iter()
        .flat_map(|(a, b)| [a.to_string().into_bytes(), b.to_string().into_bytes()])
        .collect();
    let mut argv: Vec<&[u8]> = vec![b"CLUSTER", b"MIGRATION", b"IMPORT"];
    argv.extend(bounds.iter().map(Vec::as_slice));
    let reply = c.nodes[target].link.request(&argv);
    if let Some(why) = super::migrate::failure(&reply) {
        super::migrate::report(
            c,
            &[&b"[ERR] Calling CLUSTER MIGRATION IMPORT: "[..], &why].concat(),
        );
        return false;
    }
    let task =
        reply.ok().and_then(|r| super::link::text(&r).map(<[u8]>::to_vec)).unwrap_or_default();
    if steps {
        write_out(&[&b"Waiting for migration task "[..], &task, b" to complete.\n"].concat());
    }
    wait(c, source, target, &task)
}

/// Poll the target until the task is done; cancel it when it runs too long.
fn wait(c: &mut Cluster, source: usize, target: usize, task: &[u8]) -> bool {
    let limit =
        Duration::from_millis(c.cfg.timeout_ms.map_or(DEFAULT_TIMEOUT_MS, |t| t.max(0) as u64));
    let started = Instant::now();
    loop {
        let reply =
            c.nodes[target].link.request(&[b"CLUSTER", b"MIGRATION", b"STATUS", b"ID", task]);
        let (state, last_error) = status(reply.as_ref().ok());
        match state.as_slice() {
            b"completed" => return true,
            b"failed" | b"canceled" | b"cancelled" => {
                let text = [&b"[ERR] Migration task "[..], task, b" ", &state, b": ", &last_error]
                    .concat();
                super::migrate::report(c, &text);
                return false;
            }
            _ => {}
        }
        if started.elapsed() > limit {
            for node in [target, source] {
                let _ =
                    c.nodes[node].link.request(&[b"CLUSTER", b"MIGRATION", b"CANCEL", b"ID", task]); // it may have finished meanwhile
            }
            super::migrate::report(
                c,
                &[&b"[ERR] Migration task "[..], task, b" timed out and was cancelled."].concat(),
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `state` and `last_error` of the first task in a STATUS reply.
fn status(reply: Option<&kevy_resp::Reply>) -> (Vec<u8>, Vec<u8>) {
    let Some(kevy_resp::Reply::Array(tasks)) = reply else { return Default::default() };
    let fields: Vec<&kevy_resp::Reply> = match tasks.first() {
        Some(kevy_resp::Reply::Array(f)) => f.iter().collect(),
        Some(kevy_resp::Reply::Map(m)) => m.iter().flat_map(|(k, v)| [k, v]).collect(),
        _ => return Default::default(),
    };
    let get = |name: &[u8]| {
        fields
            .chunks(2)
            .find(|kv| kv.first().and_then(|k| super::link::text(k)) == Some(name))
            .and_then(|kv| kv.get(1).and_then(|v| super::link::text(v)))
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    };
    (get(b"state"), get(b"last_error"))
}

/// Consecutive runs of slots, as inclusive ranges.
fn ranges(slots: &[u16]) -> Vec<(u16, u16)> {
    let mut out: Vec<(u16, u16)> = Vec::new();
    for &s in slots {
        match out.last_mut() {
            Some((_, last)) if *last + 1 == s => *last = s,
            _ => out.push((s, s)),
        }
    }
    out
}
