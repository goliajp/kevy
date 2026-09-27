//! Valkey's atomic slot migration (`--cluster-use-atomic-slot-migration`):
//! the source exports each group of slot ranges as one job, polled until it
//! succeeds.

use super::migrate::Progress;
use super::topology::Cluster;
use crate::rcli::send::write_out;
use kevy_resp::Reply;
use std::time::Duration;

/// One job per source, in plan order.
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
        if !one_job(c, (source, target), &slots, progress) {
            return false;
        }
        k = end;
    }
    true
}

/// `0-6,9-9`: the runs of `slots`.
pub(crate) fn ranges_text(slots: &[u16]) -> Vec<u8> {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for &s in slots {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == s => *last = s,
            _ => runs.push((s, s)),
        }
    }
    runs.iter().map(|(a, b)| format!("{a}-{b}")).collect::<Vec<_>>().join(",").into_bytes()
}

fn one_job(
    c: &mut Cluster,
    (source, target): (usize, usize),
    slots: &[u16],
    progress: Progress,
) -> bool {
    let ranges = ranges_text(slots);
    if progress == Progress::Steps {
        let text = [
            &b"Moving slot range "[..],
            &ranges,
            b" from ",
            &c.nodes[source].shown(),
            b" to ",
            &c.nodes[target].shown(),
            b" via atomic slot migration",
        ]
        .concat();
        write_out(&text);
    }
    let bounds: Vec<Vec<u8>> = ranges
        .split(|&b| b == b',')
        .flat_map(|r| r.splitn(2, |&b| b == b'-').map(<[u8]>::to_vec).collect::<Vec<_>>())
        .collect();
    let target_id = c.nodes[target].rec.id.clone();
    let mut argv: Vec<&[u8]> = vec![b"CLUSTER", b"MIGRATESLOTS"];
    for pair in bounds.chunks(2) {
        argv.push(b"SLOTSRANGE");
        argv.extend(pair.iter().map(Vec::as_slice));
    }
    argv.extend([&b"NODE"[..], &target_id]);
    if let Some(why) = super::migrate::failure(&c.nodes[source].link.request(&argv)) {
        super::migrate::move_failed(c, source, &why);
        return false;
    }
    let ok = wait(c, source, &ranges, progress);
    if ok {
        write_out(&vec![b'#'; slots.len()]);
    }
    if progress == Progress::Steps {
        write_out(b"\n");
    }
    ok
}

/// Poll the source's jobs until the newest export of `ranges` settles, a dot
/// per poll when reporting steps.
fn wait(c: &mut Cluster, source: usize, ranges: &[u8], progress: Progress) -> bool {
    loop {
        let reply = c.nodes[source].link.request(&[b"CLUSTER", b"GETSLOTMIGRATIONS"]);
        let (state, message) = job_state(reply.as_ref().ok(), ranges);
        match state.as_slice() {
            b"success" => return true,
            b"failed" | b"cancelled" | b"canceled" => {
                super::migrate::move_failed(
                    c,
                    source,
                    &[&b"slot migration "[..], &state, b": ", &message].concat(),
                );
                return false;
            }
            _ => {}
        }
        if progress == Progress::Steps {
            write_out(b".");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `state` and `message` of the first (newest) export job for `ranges`.
fn job_state(reply: Option<&Reply>, ranges: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let Some(Reply::Array(jobs)) = reply else { return Default::default() };
    for job in jobs {
        let fields: Vec<&Reply> = match job {
            Reply::Array(f) => f.iter().collect(),
            Reply::Map(m) => m.iter().flat_map(|(k, v)| [k, v]).collect(),
            _ => continue,
        };
        let get = |name: &[u8]| {
            fields
                .chunks(2)
                .find(|kv| kv.first().and_then(|k| super::link::text(k)) == Some(name))
                .and_then(|kv| kv.get(1).and_then(|v| super::link::text(v)))
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        };
        let same_ranges =
            get(b"slot_ranges").split(|&b| b == b' ' || b == b',').eq(ranges.split(|&b| b == b','));
        if get(b"operation").eq_ignore_ascii_case(b"EXPORT") && same_ranges {
            return (get(b"state"), get(b"message"));
        }
    }
    Default::default()
}
