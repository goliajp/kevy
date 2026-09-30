//! An `MSET` is logged as one frame per shard it touches, and each shard's
//! share replays whole: the values come back, a key given twice with its
//! last value, and no per-key `SET` frame is written.

#![cfg(feature = "persist")]

use kevy_embedded::{Config, Store};

fn config(dir: &std::path::Path) -> Config {
    Config::default().with_persist(dir).with_shards(4).with_mapped_aof(false)
}

#[test]
fn one_frame_per_shard_and_every_pair_replays() {
    let dir = kevy_tmpdir::TmpDir::new("mset-frames");
    let keys: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
    let vals: Vec<String> = (0..100).map(|i| format!("v{i}")).collect();
    let mut pairs: Vec<(&[u8], &[u8])> =
        keys.iter().zip(&vals).map(|(k, v)| (k.as_bytes(), v.as_bytes())).collect();
    pairs.push((b"k7", b"again"));
    {
        let s = Store::open(config(dir.path())).expect("open");
        s.mset(&pairs).expect("mset");
        drop(s);
    }
    let mut frames = (0, 0);
    for i in 0..4 {
        let log = std::fs::read(dir.path().join(format!("aof-{i}.aof"))).expect("a log per shard");
        let count = |w: &[u8]| log.windows(w.len()).filter(|x| x == &w).count();
        frames.0 += count(b"$4\r\nMSET\r\n");
        frames.1 += count(b"$3\r\nSET\r\n");
    }
    assert_eq!(frames, (4, 0), "one MSET frame per shard, no per-key SET");
    let s = Store::open(config(dir.path())).expect("reopen");
    for (i, k) in keys.iter().enumerate() {
        let want = if i == 7 { "again".to_string() } else { format!("v{i}") };
        assert_eq!(s.get(k.as_bytes()).expect("get").as_deref(), Some(want.as_bytes()), "{k}");
    }
}
