//! What the harness says when the child does not come up.
//!
//! Two primaries timed out in a parallel `cargo test --workspace` and left
//! an empty stderr log; `kevy ready timeout` was everything ten seconds of
//! waiting could be asked about, because the probe only ever connected and
//! never looked at the child. These pin the two answers apart.

use std::path::PathBuf;
use std::time::Duration;

fn config(bin: &str, dir: &str) -> kevy_chaos::HarnessConfig {
    let port = kevy_chaos::pick_free_port().expect("a port");
    let dir = std::env::temp_dir().join(format!("{dir}-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    kevy_chaos::HarnessConfig {
        kevy_bin: PathBuf::from(bin),
        spawn_timeout: Duration::from_millis(600),
        ..kevy_chaos::HarnessConfig::new(dir, port)
    }
}

#[cfg(unix)]
#[test]
fn a_child_that_exits_is_reported_as_an_exit_and_not_as_a_timeout() {
    let cfg = config("/usr/bin/false", "kevy-chaos-exits");
    let dir = cfg.data_dir.clone();
    let err = kevy_chaos::Harness::spawn(cfg).err().expect("/usr/bin/false never listens");
    let said = err.to_string();
    assert!(said.contains("exited with"), "{said}");
    assert!(said.contains("stderr empty"), "{said}");
    assert_ne!(err.kind(), std::io::ErrorKind::TimedOut, "an exit is not a timeout: {said}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_child_that_stays_up_without_listening_is_reported_as_a_timeout() {
    // Runs until killed and never binds; Harness::drop reaps it.
    let cfg = config("/usr/bin/yes", "kevy-chaos-hangs");
    let dir = cfg.data_dir.clone();
    let err = kevy_chaos::Harness::spawn(cfg).err().expect("/usr/bin/yes never listens");
    let said = err.to_string();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{said}");
    assert!(said.contains("still running after"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}
