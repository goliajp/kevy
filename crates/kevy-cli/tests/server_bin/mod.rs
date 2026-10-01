//! The `kevy` server binary these tests drive, next to the `kevy-cli` under test.

use std::path::PathBuf;
use std::process::Command;

/// `kevy`, built into the same target directory and profile as this test's
/// `kevy-cli` when it is not there yet. Built anywhere else, a coverage run
/// left an instrumented server in `target/debug` for other checks to run.
pub fn kevy_server() -> PathBuf {
    let profile_dir = std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli")).parent().unwrap();
    let bin = profile_dir.join("kevy");
    if !bin.exists() {
        let target = profile_dir.parent().unwrap();
        let profile = profile_dir.file_name().unwrap().to_string_lossy().into_owned();
        let mut build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        build.args(["build", "-p", "kevy", "--bin", "kevy", "--target-dir"]).arg(target);
        if profile != "debug" {
            build.args(["--profile", &profile]);
        }
        let status = build.status().expect("spawn cargo build");
        assert!(status.success(), "cargo build -p kevy --bin kevy failed");
    }
    bin
}
