//! `appendfsync no` and `everysec`: a write older than one reaper tick is
//! in the kernel, so it survives a SIGKILL of the process (power loss is
//! a separate matter). Each test re-runs its own binary as the victim.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kevy_embedded::{AppendFsync, Config, Store};

const VICTIM_DIR: &str = "KEVY_TICK_KILL_VICTIM_DIR";
const KEYS: usize = 10;

fn config(dir: &std::path::Path, fsync: AppendFsync) -> Config {
    Config::default().with_persist(dir).with_appendfsync(fsync)
}

fn value(i: usize) -> Vec<u8> {
    vec![b'a' + i as u8; 4096]
}

// 40 KiB of writes stay far below the 256 KiB buffer, so nothing but
// the tick can move them into the kernel before the kill
fn victim(dir: &str, fsync: AppendFsync, wait: Duration) -> ! {
    let opened = Instant::now();
    let store = Store::open(config(dir.as_ref(), fsync)).unwrap();
    for i in 0..KEYS {
        store.set(format!("k{i}").as_bytes(), &value(i)).unwrap();
    }
    std::thread::sleep(wait);
    println!("READY {}", opened.elapsed().as_millis());
    loop {
        std::thread::park();
    }
}

/// Run `test` as the victim, SIGKILL it once it is ready, and return
/// the victim's milliseconds from open to ready plus the surviving keys.
fn kill_and_count(test: &str, fsync: AppendFsync) -> (u128, usize) {
    let dir = kevy_tmpdir::unique_dir(&format!("tick-kill-{test}"));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(VICTIM_DIR, &dir)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let ready = out
        .lines()
        .map_while(Result::ok)
        .find_map(|l| l.strip_prefix("READY ").map(|ms| ms.parse::<u128>().unwrap()));
    child.kill().unwrap(); // SIGKILL: no destructor, no buffer flush
    child.wait().unwrap();
    let ms = ready.expect("the victim never finished writing");

    let store = Store::open(config(&dir, fsync)).unwrap();
    let kept = (0..KEYS)
        .filter(|&i| store.get(format!("k{i}").as_bytes()).unwrap() == Some(value(i)))
        .count();
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
    (ms, kept)
}

#[test]
fn no_mode_writes_older_than_a_tick_survive_sigkill() {
    if let Ok(dir) = std::env::var(VICTIM_DIR) {
        victim(&dir, AppendFsync::No, Duration::from_secs(1)); // ten default reaper ticks
    }
    let (_, kept) =
        kill_and_count("no_mode_writes_older_than_a_tick_survive_sigkill", AppendFsync::No);
    assert_eq!(kept, KEYS, "writes older than a tick were lost to the kill");
}

#[test]
fn everysec_writes_older_than_a_tick_survive_sigkill_before_any_fsync() {
    let name = "everysec_writes_older_than_a_tick_survive_sigkill_before_any_fsync";
    if let Ok(dir) = std::env::var(VICTIM_DIR) {
        victim(&dir, AppendFsync::EverySec, Duration::from_millis(300)); // three default ticks
    }
    let (ms, kept) = kill_and_count(name, AppendFsync::EverySec);
    // the fsync window opens a second after the log does; a kill inside
    // it means only the tick's write can have saved the records
    assert!(ms < 900, "the victim took {ms} ms, the fsync window may have closed");
    assert_eq!(kept, KEYS, "writes older than a tick were lost to the kill");
}
