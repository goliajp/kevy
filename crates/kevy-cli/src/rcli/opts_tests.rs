//! Option parsing without a process: every flag lands in the field redis-cli
//! puts it in, and every refusal is a refusal. Messages are pinned by the
//! integration test and by `bench/cligate.py`; this pins the state.

use super::super::format::Output;
use super::super::opts::Opts;
use super::{Parsed, parse};

fn args(words: &[&str]) -> Vec<Vec<u8>> {
    words.iter().map(|w| w.as_bytes().to_vec()).collect()
}

fn run(words: &[&str]) -> (Opts, usize) {
    match parse(&args(words), false) {
        Parsed::Run(o, first) => (*o, first),
        Parsed::Exit(code) => panic!("{words:?} exited {code}"),
    }
}

fn exits(words: &[&str]) -> u8 {
    match parse(&args(words), false) {
        Parsed::Exit(code) => code,
        Parsed::Run(..) => panic!("{words:?} did not exit"),
    }
}

#[test]
fn connection_flags() {
    let (o, first) = run(&[
        "-h",
        "db",
        "-p",
        "7000",
        "-t",
        "0",
        "-s",
        "/s",
        "-r",
        "-1",
        "-i",
        "0.5",
        "-n",
        "2",
        "--user",
        "u",
        "-a",
        "p",
        "--no-auth-warning",
        "--askpass",
        "-e",
        "--verbose",
        "-2",
        "--name",
        "n",
        "GET",
        "k",
    ]);
    assert_eq!((o.host.as_slice(), o.port, o.connect_timeout), (&b"db"[..], 7000, None));
    assert_eq!(
        (o.socket.as_deref(), o.repeat, o.interval_us, o.input_dbnum),
        (Some(&b"/s"[..]), -1, 500_000, 2)
    );
    assert_eq!((o.user.as_deref(), o.auth.as_deref()), (Some(&b"u"[..]), Some(&b"p"[..])));
    assert!(o.no_auth_warning && o.askpass && o.set_errcode && o.verbose && o.resp2);
    assert_eq!((o.client_name.as_deref(), first), (Some(&b"n"[..]), 25));
    let (o, _) = run(&["-t", "2.5", "-3", "-4", "-x", "-c", "--pass", "q", "--no-auth-warning"]);
    assert_eq!(
        (o.connect_timeout, o.resp3, o.prefer_ipv4, o.stdin_lastarg, o.cluster_mode),
        (Some(2.5), 1, true, true, true)
    );
    assert_eq!(run(&["-6", "-X", "T"]).0.stdin_tag.as_deref(), Some(&b"T"[..]));
    assert_eq!(exits(&["-t", "-1"]), 1);
    assert_eq!(exits(&["-v"]), 0);
    assert_eq!(exits(&["-h"]), 0);
}

#[test]
fn output_flags() {
    assert_eq!(run(&["--csv"]).0.output, Output::Csv);
    assert_eq!(run(&["--csv", "--no-raw"]).0.output, Output::Standard);
    assert_eq!(run(&["--raw"]).0.output, Output::Raw);
    let (o, _) = run(&["--json"]);
    assert_eq!((o.output, o.resp3), (Output::Json, 2));
    let (o, _) = run(&["-3", "--quoted-json"]);
    assert_eq!((o.output, o.resp3), (Output::QuotedJson, 1));
    let (o, _) = run(&["--quoted-input", "-d", ",", "-D", "|", "--show-pushes", "No"]);
    assert!(o.quoted_input && !o.push_output);
    assert_eq!((o.delims.multibulk.as_slice(), o.delims.reply.as_slice()), (&b","[..], &b"|"[..]));
    assert!(run(&["--show-pushes", "yes"]).0.push_output);
    assert!(!run(&["--show-pushes", ""]).0.push_output, "an unknown value changes nothing");
}

#[test]
fn mode_flags() {
    let (o, _) = run(&[
        "--stat",
        "--latency-dist",
        "--mono",
        "--latency-history",
        "--latency-percentiles",
        "50,99.9",
        "--vset-recall",
        "vs",
        "--vset-recall-ele",
        "0",
        "--vset-recall-count",
        "5",
        "--vset-recall-ef",
        "-2",
        "--lru-test",
        "9",
        "--replica",
        "--slave",
        "--scan",
        "--pattern",
        "a*",
        "--count",
        "7",
        "--intrinsic-latency",
        "3",
        "--rdb",
        "f",
        "--pipe",
        "--pipe-timeout",
        "4",
        "--bigkeys",
        "--memkeys",
        "--hotkeys",
        "--hotkeys-count",
        "8",
        "--keystats",
        "--cursor",
        "11",
        "--top",
        "0",
        "--eval",
        "s.lua",
        "--ldb",
        "--test_hint",
        "h",
        "--test_hint_file",
        "hf",
    ]);
    let m = &o.modes;
    assert!(
        m.stat
            && m.latency
            && m.latency_dist
            && m.mono
            && m.latency_history
            && m.replica
            && m.scan
            && m.pipe
    );
    assert!(m.bigkeys && m.memkeys && m.hotkeys && m.keystats && m.getrdb && m.eval_ldb);
    assert_eq!(m.latency_percentiles, [(50.0, b"50".to_vec()), (99.9, b"99.9".to_vec())]);
    assert_eq!(
        (m.vset_recall.as_deref(), m.vset_recall_ele, m.vset_recall_count, m.vset_recall_ef),
        (Some(&b"vs"[..]), 1, 5, 1)
    );
    assert_eq!(
        (m.lru_test, m.count, m.intrinsic_latency, m.pipe_timeout, m.hotkeys_count),
        (Some(9), 7, Some(3), 4, 8)
    );
    assert_eq!(
        (m.pattern.as_deref(), m.rdb_file.as_deref(), m.cursor, m.top),
        (Some(&b"a*"[..]), Some(&b"f"[..]), 11, 0)
    );
    assert_eq!(
        (m.eval.as_deref(), m.test_hint.as_deref(), m.test_hint_file.as_deref()),
        (Some(&b"s.lua"[..]), Some(&b"h"[..]), Some(&b"hf"[..]))
    );
    assert_eq!(o.output, Output::Raw, "--ldb forces raw output");
    let (o, _) = run(&[
        "--eval",
        "s",
        "--ldb-sync-mode",
        "--memkeys-samples",
        "5",
        "--quoted-pattern",
        "\"a\\x2a\"",
        "--functions-rdb",
        "g",
    ]);
    assert!(o.modes.eval_ldb_sync && o.modes.memkeys && o.modes.keystats && o.modes.functions_rdb);
    assert_eq!((o.modes.memkeys_samples, o.modes.pattern.as_deref()), (5, Some(&b"a*"[..])));
    assert_eq!(run(&["--keystats-samples", "6", "--cursor", "-0"]).0.modes.memkeys_samples, 6);
    assert_eq!(exits(&["--latency-percentiles", ""]), 1);
    assert_eq!(exits(&["--latency-percentiles", " 5"]), 1);
    assert_eq!(exits(&["--ldb-sync-mode"]), 1);
}

#[test]
fn cluster_flags_are_kept_raw() {
    let (o, first) = run(&[
        "--cluster",
        "reshard",
        "h:1",
        "--cluster-from",
        "a",
        "--cluster-to",
        "b",
        "--cluster-slots",
        "3",
        "--cluster-yes",
        "--cluster-weight",
        "x=1",
        "y=2",
        "--cluster-timeout",
        "9",
    ]);
    assert_eq!(o.modes.cluster, Some(args(&["reshard", "h:1"])));
    assert_eq!(first, 15, "no command: the scan ran to the end");
    let flags: Vec<(&[u8], Option<&[u8]>)> =
        o.modes.cluster_flags.iter().map(|(f, v)| (f.as_slice(), v.as_deref())).collect();
    assert_eq!(flags[0], (&b"--cluster-from"[..], Some(&b"a"[..])));
    assert!(flags.contains(&(&b"--cluster-yes"[..], None)));
    assert_eq!(flags.iter().filter(|(f, _)| *f == b"--cluster-weight").count(), 2);
    let (o, _) = run(&[
        "--cluster",
        "create",
        "--cluster-replicas",
        "1",
        "h:1",
        "h:2",
        "--cluster-slave",
        "extra",
    ]);
    assert_eq!(
        o.modes.cluster,
        Some(args(&["create", "h:1", "h:2"])),
        "late arguments join; a later run is ignored"
    );
    assert_eq!(exits(&["--cluster", "check", "h:1", "--cluster", "info"]), 1);
    assert_eq!(exits(&["--cluster-from"]), 1, "a valued flag at the end is unrecognized");
}

#[test]
fn uris() {
    let (o, _) = run(&["-u", "REDIS://alice:w%6fnder@db.example:7001/4"]);
    assert_eq!((o.host.as_slice(), o.port, o.input_dbnum), (&b"db.example"[..], 7001, 4));
    assert_eq!((o.user.as_deref(), o.auth.as_deref()), (Some(&b"alice"[..]), Some(&b"wonder"[..])));
    let (o, _) = run(&["-h", "kept", "-a", "old", "--user", "old", "-u", "redis://:pw@"]);
    assert_eq!(
        (o.host.as_slice(), o.user, o.auth.as_deref()),
        (&b"kept"[..], None, Some(&b"pw"[..]))
    );
    let (o, _) = run(&["-u", "redis://u:@[::1]:6380"]);
    assert_eq!((o.host.as_slice(), o.port, o.auth.as_deref()), (&b"::1"[..], 6380, None));
    let (o, _) = run(&["-u", "redis://[::1"]);
    assert_eq!(o.host.as_slice(), b"::1");
    let (o, _) = run(&["-u", "redis://[fe80::1]/"]);
    assert_eq!((o.host.as_slice(), o.port, o.input_dbnum), (&b"fe80::1"[..], 6379, 0));
    let (o, _) = run(&["-u", "redis:///5"]);
    assert_eq!((o.host.as_slice(), o.input_dbnum), (&b"127.0.0.1"[..], 5));
    assert_eq!(exits(&["-u", "redis://h:99999"]), 1);
    assert_eq!(exits(&["-u", "valkeys://h"]), 1);
    assert_eq!(exits(&["-u", "rediss://h"]), 1);
    // `%4` then `@`: the second hex digit is the delimiter, which is illegal.
    assert_eq!(exits(&["-u", "redis://a:%4@h"]), 1);
    // Without `@` there is no userinfo, so `%4` is a host and nothing decodes.
    assert_eq!(run(&["-u", "redis://%4"]).0.host.as_slice(), b"%4");
}

#[test]
fn latency_alone_and_a_bad_user_encoding() {
    assert!(run(&["--latency"]).0.modes.latency);
    assert_eq!(exits(&["-u", "redis://a%zz:p@h"]), 1);
}
