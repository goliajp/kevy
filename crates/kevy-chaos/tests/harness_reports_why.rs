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

/// Long enough for a probe round — one connect attempt is up to 400ms — to
/// happen at least twice, so the answer is the harness's and not the clock's.
const ROOM_FOR_TWO_ROUNDS: Duration = Duration::from_millis(2000);

fn config(bin: PathBuf, dir: &str) -> kevy_chaos::HarnessConfig {
    let port = kevy_chaos::pick_free_port().expect("a port");
    let dir = std::env::temp_dir().join(format!("{dir}-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    kevy_chaos::HarnessConfig {
        kevy_bin: bin,
        spawn_timeout: ROOM_FOR_TWO_ROUNDS,
        ..kevy_chaos::HarnessConfig::new(dir, port)
    }
}

#[test]
fn a_child_that_exits_is_reported_as_an_exit_and_not_as_a_timeout() {
    let bin = stand_in("exits", "echo 'could not bind' >&2\nexit 3");
    let cfg = config(bin.clone(), "kevy-chaos-exits");
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
    let cfg = config(bin.clone(), "kevy-chaos-hangs");
    let dir = cfg.data_dir.clone();
    let err = kevy_chaos::Harness::spawn(cfg).err().expect("the stand-in never listens");
    let said = err.to_string();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{said}");
    assert!(said.contains("still running after"), "{said}");
    assert!(said.contains("stderr empty"), "silence is a fact worth printing: {said}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&bin);
}
