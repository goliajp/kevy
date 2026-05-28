//! Zenoh 1-PUB → N-SUB fan-out bench. Same shape as kevy's
//! bench/pubsub_loopback.sh + the ZeroMQ bench (200k publishes, 50
//! subscribers, 16 B payload — reports `delivered msg/s`).
//!
//! Build: cargo build --release
//! Run:   SUBS=50 MSGS=200000 SIZE=16 ./target/release/zenoh_pubsub

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let subs_cnt: usize = env::var("SUBS").ok().and_then(|s| s.parse().ok()).unwrap_or(50);
    let msgs_cnt: usize = env::var("MSGS").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let size: usize = env::var("SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(16);
    let mode = env::var("ZMODE").unwrap_or_else(|_| "peer".to_string()); // peer / client

    // Peer mode — single host. Discovery storm with 50+ separate sessions
    // bogs down peer mesh, so we run one shared session and declare 50
    // independent subscribers on it. Each subscriber is still its own
    // task draining its own channel, so the fan-out path is exercised —
    // we just don't pay 50× transport setup. Matches the "1 publisher,
    // 50 consumers" semantic kevy / valkey / redis / zmq tested.
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", &format!("\"{}\"", mode))
        .map_err(|e| format!("zenoh config: {e}"))?;
    let session = zenoh::open(config.clone()).await?;
    let publisher = session
        .declare_publisher("kevy/bench/topic")
        .await?;

    let delivered = Arc::new(AtomicUsize::new(0));
    let mut sub_handles = Vec::with_capacity(subs_cnt);
    for _ in 0..subs_cnt {
        let sub = session.declare_subscriber("kevy/bench/topic").await?;
        let delivered = Arc::clone(&delivered);
        let h = tokio::spawn(async move {
            let mut got = 0usize;
            while got < msgs_cnt {
                match sub.recv_async().await {
                    Ok(_sample) => got += 1,
                    Err(_) => break,
                }
            }
            delivered.fetch_add(got, Ordering::Relaxed);
        });
        sub_handles.push(h);
    }

    // Give Zenoh a moment to converge subscriber routes.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let payload = vec![b'x'; size];
    let t0 = Instant::now();
    for _ in 0..msgs_cnt {
        publisher.put(payload.clone()).await?;
    }
    let pub_elapsed = t0.elapsed();

    for h in sub_handles {
        let _ = h.await;
    }
    let total_elapsed = t0.elapsed();

    let delivered_total = delivered.load(Ordering::Relaxed);
    let delivered_rate = if total_elapsed.as_secs_f64() > 0.0 {
        (delivered_total as f64 / total_elapsed.as_secs_f64()) as u64
    } else {
        0
    };
    let publish_rate = if pub_elapsed.as_secs_f64() > 0.0 {
        (msgs_cnt as f64 / pub_elapsed.as_secs_f64()) as u64
    } else {
        0
    };
    println!(
        "zenoh-pubsub mode={mode} subs={subs_cnt} msgs={msgs_cnt} size={size}B \
         delivered={delivered_rate} msg/s publishes={publish_rate}/s \
         elapsed={:.3}s pub_elapsed={:.3}s delivered_total={delivered_total}",
        total_elapsed.as_secs_f64(),
        pub_elapsed.as_secs_f64(),
    );

    Ok(())
}
