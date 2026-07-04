//! repligate primary process: embedded store with the writer source
//! enabled, continuous writes until killed.
//!
//! usage: primary_writer <source-port> <n-keys> [backlog-bytes]

use kevy_embedded::{Config, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().expect("port").parse().expect("port");
    let n_keys: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let backlog: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64 << 20);
    let mut cfg = Config::default()
        .with_shards(4)
        .with_embed_writer(format!("127.0.0.1:{port}"));
    cfg.embed_writer_backlog_bytes = backlog;
    let store = Store::open(cfg).expect("open");
    for i in 0..n_keys {
        store
            .hset(
                format!("p:{i}").as_bytes(),
                &[(b"n", format!("{i}").as_bytes()), (b"s", b"seeded")],
            )
            .expect("seed");
    }
    // a few non-hash rows so the snapshot covers types
    store.set(b"p:greeting", b"hello").expect("set");
    store.rpush(b"p:list", &[b"a", b"b"]).expect("rpush");
    store.zadd(b"p:zset", &[(1.5, b"m" as &[u8])]).expect("zadd");
    println!("READY");
    let mut i = 0u64;
    loop {
        let k = (i.wrapping_mul(2654435761)) as usize % n_keys;
        store
            .hset(format!("p:{k}").as_bytes(), &[(b"s", format!("u{i}").as_bytes())])
            .expect("update");
        i += 1;
        if i.is_multiple_of(2048) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
