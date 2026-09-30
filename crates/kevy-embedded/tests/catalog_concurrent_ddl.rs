//! Catalog methods called at the same moment from several threads all
//! take effect: each one's change is in the catalog, in the log it comes
//! back from after a reopen, and on a replica.

#![cfg(all(feature = "index", feature = "replicate", not(target_arch = "wasm32")))]

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};
use kevy_tmpdir::TmpDir;

const THREADS: usize = 4;
/// Rounds each thread declares: two indexes a round, and the catalog
/// holds at most 64.
const ROUNDS: usize = 7;

fn call(s: &Store, cmd: &str) -> Vec<u8> {
    let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    out
}

/// The names a `*.LIST` reply holds, sorted.
fn names(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply);
    let lines: Vec<&str> = text.split("\r\n").collect();
    let mut names: Vec<String> = lines
        .windows(3)
        .filter(|w| w[0] == "name" && w[1].starts_with('$'))
        .map(|w| w[2].to_string())
        .collect();
    names.sort();
    names
}

fn ddl(thread: usize, round: usize) -> [String; 3] {
    let tag = format!("c{thread}r{round}");
    [
        format!("IDX.CREATE i{tag} ON PREFIX p{tag}: FIELD age TYPE i64 KIND range"),
        format!("VIEW.CREATE v{tag} QUERY i{tag} RANGE 0 10 ORDER BY i{tag}"),
        format!(
            "TABLE.DECLARE t{tag} PREFIX t{tag}: PK id COLUMN id i64 COLUMN n i64 INDEX n range"
        ),
    ]
}

/// Every thread declares at once; the refused commands with their replies.
fn declare_at_once(s: &Store) -> Vec<String> {
    let barrier = Arc::new(Barrier::new(THREADS));
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let (s, barrier) = (s.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let mut refused = Vec::new();
                for round in 0..ROUNDS {
                    for cmd in ddl(thread, round) {
                        let reply = call(&s, &cmd);
                        if reply != b"+OK\r\n" {
                            refused.push(format!("{cmd}: {}", String::from_utf8_lossy(&reply)));
                        }
                    }
                }
                refused
            })
        })
        .collect();
    threads.into_iter().flat_map(|t| t.join().unwrap()).collect()
}

fn declared() -> [Vec<String>; 3] {
    let mut want: [Vec<String>; 3] = Default::default();
    for thread in 0..THREADS {
        for round in 0..ROUNDS {
            let tag = format!("c{thread}r{round}");
            want[0].push(format!("i{tag}"));
            want[0].push(format!("t{tag}.n"));
            want[1].push(format!("v{tag}"));
            want[2].push(format!("t{tag}"));
        }
    }
    want.iter_mut().for_each(|w| w.sort());
    want
}

fn catalog(s: &Store) -> [Vec<String>; 3] {
    ["IDX.LIST", "VIEW.LIST", "TABLE.LIST"].map(|list| names(&call(s, list)))
}

fn settled(s: &Store, want: &[Vec<String>; 3]) -> [Vec<String>; 3] {
    let started = Instant::now();
    let mut got = catalog(s);
    while &got != want && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
        got = catalog(s);
    }
    got
}

fn missing(got: &[Vec<String>; 3], want: &[Vec<String>; 3]) -> Vec<String> {
    (0..3).flat_map(|k| want[k].iter().filter(move |n| !got[k].contains(n)).cloned()).collect()
}

#[test]
fn catalog_methods_at_once_from_every_thread_all_take_effect() {
    let dir = TmpDir::new("emb-ddl-race");
    let config = || Config::default().with_persist(dir.path()).with_shards(4);
    let primary = Store::open(config().with_embed_writer("127.0.0.1:0")).unwrap();
    let addr = primary.writer_addr().unwrap().to_string();
    let replica = Store::open_replica(&addr).unwrap();
    let refused = declare_at_once(&primary);
    let want = declared();
    let got = catalog(&primary);
    let lost = missing(&got, &want);
    assert!(
        got == want && refused.is_empty(),
        "the store lost {lost:?}; {} refused, first {:?}",
        refused.len(),
        refused.first()
    );
    let got = settled(&replica, &want);
    assert!(got == want, "the replica lost {:?}", missing(&got, &want));
    drop(replica);
    drop(primary);
    let reopened = Store::open(config()).unwrap();
    let got = catalog(&reopened);
    assert!(got == want, "the reopen lost {:?}", missing(&got, &want));
}
