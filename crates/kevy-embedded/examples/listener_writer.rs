//! topogate writer process: embedded store + read-only listener,
//! continuous write load until killed.
//!
//! usage: listener_writer <port> [rate-keys]

use kevy_embedded::{Config, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().expect("port").parse().expect("port");
    let n_keys: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let addr = format!("127.0.0.1:{port}").parse().expect("addr");
    let store = Store::open(
        Config::default().with_shards(4).with_feed(1 << 22).with_resp_listener(addr),
    )
    .expect("open");
    // seed
    for i in 0..n_keys {
        store
            .hset(
                format!("row:{i}").as_bytes(),
                &[(b"n", format!("{i}").as_bytes()), (b"s", b"seeded")],
            )
            .expect("seed");
    }
    println!("READY");
    // continuous update load
    let mut i = 0u64;
    loop {
        let k = (i.wrapping_mul(2654435761)) as usize % n_keys;
        store
            .hset(format!("row:{k}").as_bytes(), &[(b"s", format!("u{i}").as_bytes())])
            .expect("update");
        i += 1;
        if i % 4096 == 0 {
            std::thread::yield_now();
        }
    }
}
