//! Which special mode runs: the first enabled one, in redis-cli's order.

use super::sizes::Measure;
use crate::rcli::opts::Opts;
use crate::rcli::session::{Connect, Session, eprint_bytes};

/// The enabled special modes, in the order they take precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Cluster,
    Latency,
    LatencyDist,
    VsetRecall,
    Replica,
    Rdb,
    Pipe,
    BigKeys,
    MemKeys,
    KeyStats,
    HotKeys,
    Stat,
    Scan,
    LruTest,
    IntrinsicLatency,
    ClusterRedirects,
}

/// Run the first special mode `s.opts` enables; `None` when there is none.
pub(crate) fn run(s: &mut Session) -> Option<u8> {
    let (mode, flag) = first_mode(&s.opts)?;
    let needs_server = matches!(
        mode,
        Mode::Scan
            | Mode::BigKeys
            | Mode::MemKeys
            | Mode::KeyStats
            | Mode::HotKeys
            | Mode::Stat
            | Mode::Latency
            | Mode::LatencyDist
            | Mode::VsetRecall
            | Mode::LruTest
            | Mode::Pipe
    );
    if needs_server && !s.connect(Connect::Report) {
        return Some(1);
    }
    Some(match mode {
        Mode::Scan => super::scan::run(s),
        Mode::HotKeys => super::hotkeys::run(s),
        Mode::KeyStats => super::keystats::run(s),
        Mode::Stat => super::stat::run(s),
        Mode::Pipe => super::pipe::run(s),
        Mode::Latency => super::latency::run(s),
        Mode::LatencyDist => super::latency_dist::run(s),
        Mode::VsetRecall => {
            let key = s.opts.modes.vset_recall.clone().unwrap_or_default();
            super::vset_recall::run(s, &key)
        }
        Mode::LruTest => super::lru_test::run(s, s.opts.modes.lru_test.unwrap_or(0)),
        Mode::IntrinsicLatency => {
            super::intrinsic::run(s.opts.modes.intrinsic_latency.unwrap_or(0))
        }
        Mode::BigKeys => super::bigkeys::run(s, Measure::Length),
        Mode::MemKeys => {
            super::bigkeys::run(s, Measure::Memory { samples: s.opts.modes.memkeys_samples })
        }
        _ => {
            eprint_bytes(&[b"kevy-cli: ", flag.as_bytes(), b" is not implemented yet\n"]);
            1
        }
    })
}

// LOC-WAIVER: a table — one row per mode flag, in redis-cli's precedence.
fn first_mode(o: &Opts) -> Option<(Mode, &'static str)> {
    let m = &o.modes;
    [
        (m.cluster.is_some(), Mode::Cluster, "--cluster"),
        (m.latency, Mode::Latency, "--latency"),
        (m.latency_dist, Mode::LatencyDist, "--latency-dist"),
        (m.vset_recall.is_some(), Mode::VsetRecall, "--vset-recall"),
        (m.replica, Mode::Replica, "--replica"),
        (m.getrdb || m.functions_rdb, Mode::Rdb, "--rdb"),
        (m.pipe, Mode::Pipe, "--pipe"),
        (m.bigkeys, Mode::BigKeys, "--bigkeys"),
        (m.memkeys, Mode::MemKeys, "--memkeys"),
        (m.keystats, Mode::KeyStats, "--keystats"),
        (m.hotkeys, Mode::HotKeys, "--hotkeys"),
        (m.stat, Mode::Stat, "--stat"),
        (m.scan, Mode::Scan, "--scan"),
        (m.lru_test.is_some(), Mode::LruTest, "--lru-test"),
        (m.intrinsic_latency.is_some(), Mode::IntrinsicLatency, "--intrinsic-latency"),
        (o.cluster_mode, Mode::ClusterRedirects, "-c"),
    ]
    .into_iter()
    .find_map(|(on, mode, flag)| on.then_some((mode, flag)))
}
