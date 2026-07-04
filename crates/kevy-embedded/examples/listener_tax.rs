//! topogate zero-tax clamp: embedded write throughput, listener
//! enabled-but-idle vs off — must be within 10%.

use std::time::Instant;

use kevy_embedded::{Config, Store};

fn burst(store: &Store, rounds: u64) -> f64 {
    let t0 = Instant::now();
    for i in 0..rounds {
        let k = (i.wrapping_mul(2654435761)) % 50_000;
        store
            .hset(format!("t:{k}").as_bytes(), &[(b"v", format!("{i}").as_bytes())])
            .expect("write");
    }
    rounds as f64 / t0.elapsed().as_secs_f64()
}

fn main() {
    let off = Store::open(Config::default().with_shards(4)).expect("open");
    let _ = burst(&off, 100_000); // warm
    let base = burst(&off, 400_000);
    drop(off);

    let addr = "127.0.0.1:0".parse().expect("addr");
    let on = Store::open(Config::default().with_shards(4).with_resp_listener(addr)).expect("open");
    let _ = burst(&on, 100_000);
    let with_idle = burst(&on, 400_000);

    let tax = (base - with_idle) / base * 100.0;
    println!("topogate: write tax off={base:.0}/s idle-listener={with_idle:.0}/s tax={tax:.1}%");
    if tax >= 10.0 {
        println!("topogate: FAIL — idle listener tax {tax:.1}% >= 10%");
        std::process::exit(1);
    }
    println!("topogate: PASS");
}
