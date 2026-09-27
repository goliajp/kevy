//! `appendfsync no`: a write older than one reaper tick is in the
//! kernel, so it survives a SIGKILL of the process (power loss is still
//! up to the OS). The test re-runs its own binary as the victim.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use kevy_embedded::{AppendFsync, Config, Store};

const VICTIM_DIR: &str = "KEVY_NO_MODE_VICTIM_DIR";
const KEYS: usize = 10;

fn config(dir: &std::path::Path) -> Config {
    Config::default().with_persist(dir).with_appendfsync(AppendFsync::No)
}

fn value(i: usize) -> Vec<u8> {
    vec![b'a' + i as u8; 4096]
}

// 40 KiB of writes stay far below the 256 KiB buffer, so nothing but
// the tick can move them into the kernel before the kill
fn victim(dir: &str) -> ! {
    let store = Store::open(config(dir.as_ref())).unwrap();
    for i in 0..KEYS {
        store.set(format!("k{i}").as_bytes(), &value(i)).unwrap();
    }
    std::thread::sleep(Duration::from_secs(1)); // ten default reaper ticks
    println!("READY");
    loop {
        std::thread::park();
    }
}

#[test]
fn no_mode_writes_older_than_a_tick_survive_sigkill() {
    if let Ok(dir) = std::env::var(VICTIM_DIR) {
        victim(&dir);
    }
    let uniq =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("kevy-no-mode-kill-{uniq}"));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "no_mode_writes_older_than_a_tick_survive_sigkill", "--nocapture"])
        .env(VICTIM_DIR, &dir)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let ready = out.lines().map_while(Result::ok).any(|l| l == "READY");
    child.kill().unwrap(); // SIGKILL: no destructor, no buffer flush
    child.wait().unwrap();
    assert!(ready, "the victim never finished writing");

    let store = Store::open(config(&dir)).unwrap();
    let kept = (0..KEYS)
        .filter(|&i| store.get(format!("k{i}").as_bytes()).unwrap() == Some(value(i)))
        .count();
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(kept, KEYS, "writes older than a tick were lost to the kill");
}
