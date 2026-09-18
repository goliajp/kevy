//! What the harness says when the child does not come up.
//!
//! Two primaries timed out in a parallel `cargo test --workspace` and left
//! an empty stderr log; `kevy ready timeout` was everything ten seconds of
//! waiting could be asked about, because the probe only ever connected and
//! never looked at the child. These pin the two answers apart.
//!
//! The stand-ins are written here rather than borrowed from the system,
//! because the harness passes `--config <path>` to whatever it starts and
//! what a program does with an option it does not know is not portable:
//! `/usr/bin/yes` ignores it on BSD and exits 1 on GNU, which is how the
//! first version of this file passed on macOS and failed on Linux.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

/// An executable `/bin/sh` script that ignores its arguments.
fn stand_in(name: &str, body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kevy-chaos-{name}-{}.sh", std::process::id()));
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the stand-in");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// The two tests want opposite things from the deadline, which is why they
/// do not share one.
///
/// The exit case only has to outlast the child: the harness returns the
/// moment it sees the exit, so a generous ceiling costs nothing and a tight
/// one turns a busy machine into a wrong answer. It did — at 2s, under a load
/// average of 22, `/bin/sh` had not been scheduled long enough to run `echo`
/// and `exit 3`, so the harness reported the timeout it was being tested for
/// NOT reporting. Ten seconds is the harness's own default.
const LONGER_THAN_A_CHILD_TAKES_TO_DIE: Duration = Duration::from_secs(10);
/// The timeout case has to wait its deadline out, every time, so it is short.
/// It is satisfied by any child that is alive and not listening — one that is
/// sleeping and one that is starved of CPU look the same to it, and both are
/// the thing it asks about.
const SHORT_ENOUGH_TO_WAIT_OUT: Duration = Duration::from_millis(1500);

fn config(bin: PathBuf, dir: &str, spawn_timeout: Duration) -> kevy_chaos::HarnessConfig {
    let port = kevy_chaos::pick_free_port();
    let dir = std::env::temp_dir().join(format!("{dir}-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    kevy_chaos::HarnessConfig {
        kevy_bin: bin,
        spawn_timeout,
        ..kevy_chaos::HarnessConfig::new(dir, port)
    }
}

#[test]
fn a_child_that_exits_is_reported_as_an_exit_and_not_as_a_timeout() {
    let bin = stand_in("exits", "echo 'could not bind' >&2\nexit 3");
    let cfg = config(bin.clone(), "kevy-chaos-exits", LONGER_THAN_A_CHILD_TAKES_TO_DIE);
    let dir = cfg.data_dir.clone();
    let err = kevy_chaos::Harness::spawn(cfg).err().expect("the stand-in never listens");
    let said = err.to_string();
    assert_ne!(err.kind(), std::io::ErrorKind::TimedOut, "an exit is not a timeout: {said}");
    assert!(said.contains("exited with"), "{said}");
    assert!(said.contains('3'), "the status, so the reader knows which exit: {said}");
    assert!(said.contains("could not bind"), "what the child said before it went: {said}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&bin);
}

#[test]
fn a_child_that_stays_up_without_listening_is_reported_as_a_timeout() {
    // `exec`, so killing the child kills the sleep rather than orphaning it.
    let bin = stand_in("hangs", "exec sleep 60");
    let cfg = config(bin.clone(), "kevy-chaos-hangs", SHORT_ENOUGH_TO_WAIT_OUT);
    let dir = cfg.data_dir.clone();
    let err = kevy_chaos::Harness::spawn(cfg).err().expect("the stand-in never listens");
    let said = err.to_string();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{said}");
    assert!(said.contains("still running after"), "{said}");
    assert!(said.contains("stderr empty"), "silence is a fact worth printing: {said}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&bin);
}
