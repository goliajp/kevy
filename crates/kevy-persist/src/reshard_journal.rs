//! The reshard commit record: written durably at the commit point, read
//! back by recovery to roll an interrupted reshard forward.

use crate::{Routing, ShardsMeta};
use std::io;
use std::path::Path;

pub(crate) const JOURNAL: &str = "reshard.journal";

/// Persist the reshard commit record durably (write + fsync) — once this
/// exists, the migration is committed and any crash is rolled *forward*.
pub(crate) fn write_journal(
    dir: &Path,
    prev_n: usize,
    target: ShardsMeta,
    stamp: u128,
) -> io::Result<()> {
    use std::io::Write;
    let routing = target.routing.tag();
    let body = format!(
        "kevy-reshard-journal v1\nstamp={stamp}\nprev_n={prev_n}\nn={}\nrouting={routing}\n",
        target.n,
    );
    let mut f = std::fs::File::create(dir.join(JOURNAL))?;
    f.write_all(body.as_bytes())?;
    f.sync_all()
}

pub(crate) fn parse_journal(body: &str) -> Option<(usize, ShardsMeta, u128)> {
    let mut lines = body.lines();
    if lines.next() != Some("kevy-reshard-journal v1") {
        return None;
    }
    let mut stamp = None;
    let mut prev_n = None;
    let mut n = None;
    let mut routing = None;
    for line in lines {
        let (k, v) = line.split_once('=')?;
        match k {
            "stamp" => stamp = v.parse::<u128>().ok(),
            "prev_n" => prev_n = v.parse::<usize>().ok(),
            "n" => n = v.parse::<usize>().ok(),
            "routing" => {
                routing = match v {
                    "kevyhash" => Some(Routing::KevyHash),
                    "slots" => Some(Routing::Slots),
                    _ => None,
                }
            }
            _ => return None,
        }
    }
    Some((prev_n?, ShardsMeta { n: n?, routing: routing? }, stamp?))
}
