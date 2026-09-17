//! Which special mode runs: the first enabled one, in redis-cli's order.

use super::sizes::Measure;
use crate::rcli::opts::Opts;
use crate::rcli::session::{Connect, Session};

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
}

/// Run the first special mode `s.opts` enables; `None` when there is none.
pub(crate) fn run(s: &mut Session) -> Option<u8> {
    let mode = first_mode(&s.opts)?;
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
            | Mode::Rdb
            | Mode::Replica
    );
    if needs_server && !s.connect(Connect::Report) {
        return Some(1);
    }
    Some(match mode {
        Mode::Cluster => crate::rcli::cluster::run(s),
        Mode::Scan => super::scan::run(s),
        Mode::HotKeys => super::hotkeys::run(s),
        Mode::KeyStats => super::keystats::run(s),
        Mode::Stat => super::stat::run(s),
        Mode::Pipe => super::pipe::run(s),
        Mode::Rdb => super::rdb::run(s, s.opts.modes.functions_rdb),
        Mode::Replica => super::replica::run(s),
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
    })
}

// LOC-WAIVER: a table — one row per mode flag, in redis-cli's precedence.
/// Whether any special mode, `--eval` or a `--test-hint*` is enabled.
pub(crate) fn any(o: &Opts) -> bool {
    first_mode(o).is_some()
        || o.modes.eval.is_some()
        || o.modes.test_hint.is_some()
        || o.modes.test_hint_file.is_some()
}

fn first_mode(o: &Opts) -> Option<Mode> {
    let m = &o.modes;
    [
        (m.cluster.is_some(), Mode::Cluster),
        (m.latency, Mode::Latency),
        (m.latency_dist, Mode::LatencyDist),
        (m.vset_recall.is_some(), Mode::VsetRecall),
        (m.replica, Mode::Replica),
        (m.getrdb || m.functions_rdb, Mode::Rdb),
        (m.pipe, Mode::Pipe),
        (m.bigkeys, Mode::BigKeys),
        (m.memkeys, Mode::MemKeys),
        (m.keystats, Mode::KeyStats),
        (m.hotkeys, Mode::HotKeys),
        (m.stat, Mode::Stat),
        (m.scan, Mode::Scan),
        (m.lru_test.is_some(), Mode::LruTest),
        (m.intrinsic_latency.is_some(), Mode::IntrinsicLatency),
    ]
    .into_iter()
    .find_map(|(on, mode)| on.then_some(mode))
}
