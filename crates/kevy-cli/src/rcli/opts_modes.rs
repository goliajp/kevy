//! The special-mode, cluster-manager and TLS flags of `parseOptions`.

use super::cnum::{atoi, atoll, strtoll};
use super::help::usage;
use super::opts::Opts;
use super::opts_parse::{Step, fail, unquote, value};

// LOC-WAIVER: a dispatch table — one arm per special-mode flag.
pub(crate) fn mode_flag(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Option<Step> {
    let m = &mut o.modes;
    let v = value(argv, i);
    let flag = argv[i].as_slice();
    let takes_value = match (flag, v) {
        (b"--stat", _) => {
            m.stat = true;
            false
        }
        (b"--latency", _) => {
            m.latency = true;
            false
        }
        (b"--latency-dist", _) => {
            m.latency_dist = true;
            false
        }
        (b"--mono", _) => {
            m.mono = true;
            false
        }
        (b"--latency-history", _) => {
            m.latency = true;
            m.latency_history = true;
            false
        }
        (b"--latency-percentiles", Some(list)) => {
            return Some(percentiles(o, list).unwrap_or(Step::Next(i + 2)));
        }
        (b"--vset-recall", Some(k)) => {
            m.vset_recall = Some(k.to_vec());
            true
        }
        (b"--vset-recall-ele", Some(n)) => {
            m.vset_recall_ele = atoll(n).max(1);
            true
        }
        (b"--vset-recall-count", Some(n)) => {
            m.vset_recall_count = atoll(n).max(1);
            true
        }
        (b"--vset-recall-ef", Some(n)) => {
            m.vset_recall_ef = atoll(n).max(1);
            true
        }
        (b"--lru-test", Some(n)) => {
            m.lru_test = Some(atoll(n));
            true
        }
        (b"--slave" | b"--replica", _) => {
            m.replica = true;
            false
        }
        (b"--scan", _) => {
            m.scan = true;
            false
        }
        (b"--pattern", Some(p)) => {
            m.pattern = Some(p.to_vec());
            true
        }
        (b"--count", Some(n)) => {
            m.count = atoi(n);
            true
        }
        (b"--quoted-pattern", Some(p)) => match unquote(p) {
            Some(pat) => {
                m.pattern = Some(pat);
                true
            }
            None => return Some(fail(&[b"Invalid quoted string specified for --quoted-pattern."])),
        },
        (b"--intrinsic-latency", Some(n)) => {
            m.intrinsic_latency = Some(atoi(n));
            true
        }
        (b"--rdb", Some(f)) => {
            m.getrdb = true;
            m.rdb_file = Some(f.to_vec());
            true
        }
        (b"--functions-rdb", Some(f)) => {
            m.functions_rdb = true;
            m.rdb_file = Some(f.to_vec());
            true
        }
        (b"--pipe", _) => {
            m.pipe = true;
            false
        }
        (b"--pipe-timeout", Some(n)) => {
            m.pipe_timeout = atoi(n);
            true
        }
        (b"--bigkeys", _) => {
            m.bigkeys = true;
            false
        }
        (b"--memkeys", _) => {
            m.memkeys = true;
            m.memkeys_samples = -1;
            false
        }
        (b"--memkeys-samples", Some(n)) => {
            m.memkeys = true;
            m.keystats = true;
            return Some(samples(o, n, "--memkeys-samples").unwrap_or(Step::Next(i + 2)));
        }
        (b"--hotkeys", _) => {
            m.hotkeys = true;
            false
        }
        (b"--hotkeys-count", Some(n)) => {
            m.hotkeys_count = atoi(n);
            true
        }
        (b"--keystats", _) => {
            m.keystats = true;
            m.memkeys_samples = -1;
            false
        }
        (b"--keystats-samples", Some(n)) => {
            m.keystats = true;
            return Some(samples(o, n, "--keystats-samples").unwrap_or(Step::Next(i + 2)));
        }
        (b"--cursor", Some(n)) => match unsigned(n, "--cursor") {
            Ok(c) => {
                m.cursor = c;
                true
            }
            Err(step) => return Some(step),
        },
        (b"--top", Some(n)) => match unsigned(n, "--top") {
            Ok(t) => {
                m.top = t;
                true
            }
            Err(step) => return Some(step),
        },
        (b"--eval", Some(f)) => {
            m.eval = Some(f.to_vec());
            true
        }
        (b"--ldb", _) => {
            m.eval_ldb = true;
            o.output = super::format::Output::Raw;
            false
        }
        (b"--ldb-sync-mode", _) => {
            m.eval_ldb = true;
            m.eval_ldb_sync = true;
            o.output = super::format::Output::Raw;
            false
        }
        (b"--test_hint", Some(h)) => {
            m.test_hint = Some(h.to_vec());
            true
        }
        (b"--test_hint_file", Some(f)) => {
            m.test_hint_file = Some(f.to_vec());
            true
        }
        _ => return None,
    };
    Some(Step::Next(i + if takes_value { 2 } else { 1 }))
}

/// `--latency-percentiles 50,99,99.9`.
fn percentiles(o: &mut Opts, list: &[u8]) -> Option<Step> {
    o.modes.latency = true;
    if list.is_empty() {
        return Some(fail(&[b"Invalid --latency-percentiles list."]));
    }
    for token in list.split(|&b| b == b',') {
        match super::cnum::strtod_full(token) {
            Some(p)
                if !p.is_nan() && (0.0..=100.0).contains(&p) && !token[0].is_ascii_whitespace() =>
            {
                o.modes.latency_percentiles.push((p, token.to_vec()));
            }
            _ => {
                return Some(fail(&[
                    b"Invalid percentile '",
                    token,
                    b"' in --latency-percentiles (must be a number between 0 and 100).",
                ]));
            }
        }
    }
    None
}

/// `--memkeys-samples` / `--keystats-samples`: `strtoll` to the end, ≥ 0.
fn samples(o: &mut Opts, n: &[u8], flag: &str) -> Option<Step> {
    let (value, used) = strtoll(n);
    if used != n.len() {
        return Some(fail(&[flag.as_bytes(), b" conversion error."]));
    }
    if value < 0 {
        return Some(fail(&[flag.as_bytes(), b" value should be positive."]));
    }
    o.modes.memkeys_samples = value;
    None
}

/// `--cursor` / `--top`: `strtoull` to the end; a leading `-` only for 0.
fn unsigned(n: &[u8], flag: &str) -> Result<u64, Step> {
    let (value, used) = strtoll(n);
    if used != n.len() {
        return Err(fail(&[flag.as_bytes(), b" conversion error."]));
    }
    if n.first() == Some(&b'-') && value != 0 {
        return Err(fail(&[flag.as_bytes(), b" should be followed by a positive integer."]));
    }
    Ok(value as u64)
}

/// `--cluster-*` flags without a value.
const SWITCHES: &[&[u8]] = &[
    b"--cluster-only-masters",
    b"--cluster-only-replicas",
    b"--cluster-from-askpass",
    b"--cluster-yes",
    b"--cluster-simulate",
    b"--cluster-replace",
    b"--cluster-copy",
    b"--cluster-slave",
    b"--cluster-use-empty-masters",
    b"--cluster-search-multiple-owners",
    b"--cluster-fix-with-unreachable-masters",
    // valkey-cli's names for the same switches, and its atomic migration.
    b"--cluster-only-primaries",
    b"--cluster-replica",
    b"--cluster-use-empty-primaries",
    b"--cluster-fix-with-unreachable-primaries",
    b"--cluster-use-atomic-slot-migration",
];
/// `--cluster-*` flags that take one value.
const VALUED: &[&[u8]] = &[
    b"--cluster-replicas",
    b"--cluster-master-id",
    b"--cluster-primary-id",
    b"--cluster-from",
    b"--cluster-to",
    b"--cluster-from-user",
    b"--cluster-from-pass",
    b"--cluster-slots",
    b"--cluster-timeout",
    b"--cluster-pipeline",
    b"--cluster-threshold",
];

/// `--cluster <cmd> [args]` and the `--cluster-*` flags, stored raw for P4.
pub(crate) fn cluster_flag(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Option<Step> {
    let flag = argv[i].as_slice();
    if flag == b"--cluster" {
        if o.modes.cluster.is_some() || value(argv, i).is_none() {
            return Some(Step::Exit(usage(1)));
        }
        let mut j = i + 1;
        while j < argv.len() && argv[j].first() != Some(&b'-') {
            j += 1;
        }
        o.modes.cluster = Some(argv[i + 1..j].to_vec());
        return Some(Step::Next(j));
    }
    if SWITCHES.contains(&flag) {
        o.modes.cluster_flags.push((flag.to_vec(), None));
        return Some(Step::Next(i + 1));
    }
    if flag == b"--cluster-weight" && value(argv, i).is_some() {
        return Some(weights(o, argv, i));
    }
    let v = value(argv, i)?;
    VALUED.contains(&flag).then(|| {
        o.modes.cluster_flags.push((flag.to_vec(), Some(v.to_vec())));
        Step::Next(i + 2)
    })
}

/// `--cluster-weight n1=w n2=w`: consumes the following `=` arguments.
fn weights(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Step {
    if o.modes.cluster_flags.iter().any(|(f, _)| f == b"--cluster-weight") {
        return fail(&[b"WARNING: you cannot use --cluster-weight more than once.\nYou can set more weights by adding them as a space-separated list, ie:\n--cluster-weight n1=w n2=w"]);
    }
    let mut j = i + 1;
    while j < argv.len() && !argv[j].starts_with(b"--") && argv[j].contains(&b'=') {
        o.modes.cluster_flags.push((b"--cluster-weight".to_vec(), Some(argv[j].clone())));
        j += 1;
    }
    Step::Next(j)
}

/// With `--cluster <cmd>` given no arguments, the first later run of
/// non-dash arguments becomes its arguments.
pub(crate) fn late_cluster_args(o: &mut Opts, argv: &[Vec<u8>], i: usize) -> Step {
    let mut j = i;
    while j < argv.len() && argv[j].first() != Some(&b'-') {
        j += 1;
    }
    if let Some(cmd) = o.modes.cluster.as_mut() {
        cmd.extend(argv[i..j].iter().cloned());
    }
    Step::Next(j)
}

/// DEV-006: TLS is not implemented (owner decision D1), so its flags are
/// refused by name rather than reported as unknown or silently ignored.
pub(crate) fn tls_flag(flag: &[u8]) -> Option<Step> {
    const TLS: &[&[u8]] = &[
        b"--tls",
        b"--sni",
        b"--cacertdir",
        b"--cacert",
        b"--cert",
        b"--key",
        b"--tls-ciphers",
        b"--tls-ciphersuites",
        b"--insecure",
    ];
    TLS.contains(&flag).then(|| {
        fail(&[b"kevy-cli: ", flag, b" is not supported: kevy-cli does not implement TLS"])
    })
}
