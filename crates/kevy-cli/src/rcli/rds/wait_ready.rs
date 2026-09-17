//! `wait-ready [--index name | --table t | --all] [--timeout s]`: poll the
//! index catalog until the chosen indexes have finished building.

use super::options::Common;
use super::rows::{Cell, Rows, from_pair_rows};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use std::time::{Duration, Instant};

/// Which indexes to wait for.
enum Which {
    Index(Vec<u8>),
    Table(Vec<u8>),
    All,
}

/// Run `wait-ready`; the exit code (1 on timeout or a lost connection).
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some((which, timeout)) = parse(&common.args) else { return 1 };
    let started = Instant::now();
    let mut last = Vec::new();
    loop {
        let Some(rows) = s.request(&[b"IDX.LIST"]).ok().as_ref().and_then(from_pair_rows) else {
            eprint_bytes(&[b"kevy-cli: IDX.LIST did not answer with the index catalog\n"]);
            return 1;
        };
        let pending = pending(&rows, &which);
        if pending.is_empty() {
            write_out(b"ready\n");
            return 0;
        }
        if pending != last {
            write_out(&[&b"waiting for "[..], &pending.join(&b' '), b"\n"].concat());
            last = pending;
        }
        if timeout.is_some_and(|t| started.elapsed() >= t) {
            eprint_bytes(&[b"kevy-cli: wait-ready timed out\n"]);
            return 1;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn parse(args: &[Vec<u8>]) -> Option<(Which, Option<Duration>)> {
    let (mut which, mut timeout) = (Which::All, None);
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1);
        match (args[i].as_slice(), value) {
            (b"--index", Some(v)) => which = Which::Index(v.clone()),
            (b"--table", Some(v)) => which = Which::Table(v.clone()),
            (b"--timeout", Some(v)) => {
                // `inf` and `NaN` parse as f64; a Duration cannot hold them.
                let secs = std::str::from_utf8(v).ok().and_then(|t| t.parse::<f64>().ok());
                let Some(secs) = secs.filter(|s| s.is_finite()) else {
                    eprint_bytes(&[
                        b"kevy-cli: wait-ready: --timeout takes seconds, not '",
                        v,
                        b"'\n",
                    ]);
                    return None;
                };
                timeout = Some(Duration::from_secs_f64(secs.max(0.0)));
            }
            (b"--all", _) => {
                which = Which::All;
                i += 1;
                continue;
            }
            (other, _) => {
                eprint_bytes(&[
                    b"kevy-cli: wait-ready: unexpected '",
                    other,
                    b"' (--index name | --table t | --all, --timeout s)\n",
                ]);
                return None;
            }
        }
        i += 2;
    }
    Some((which, timeout))
}

/// Names of the chosen indexes whose state is not `ready` (a named index
/// that does not exist yet counts as pending).
fn pending(rows: &Rows, which: &Which) -> Vec<Vec<u8>> {
    let col = |name: &[u8]| rows.columns.iter().position(|c| c == name);
    let (Some(name_at), Some(state_at)) = (col(b"name"), col(b"state")) else {
        // An empty catalog: a named index is not there yet.
        return match which {
            Which::Index(n) => vec![n.clone()],
            _ => Vec::new(),
        };
    };
    let text = |cell: &Cell| match cell {
        Cell::Text(t) => t.clone(),
        _ => Vec::new(),
    };
    let chosen = |name: &[u8]| match which {
        Which::Index(n) => name == n.as_slice(),
        Which::Table(t) => name.starts_with(&[t.as_slice(), b"."].concat()),
        Which::All => true,
    };
    let mut out: Vec<Vec<u8>> = rows
        .rows
        .iter()
        .filter(|r| chosen(&text(&r[name_at])) && text(&r[state_at]) != b"ready")
        .map(|r| text(&r[name_at]))
        .collect();
    if let Which::Index(n) = which
        && !rows.rows.iter().any(|r| text(&r[name_at]) == *n)
    {
        out.push(n.clone());
    }
    out
}
