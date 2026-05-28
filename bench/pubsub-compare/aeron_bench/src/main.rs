//! Aeron 1-PUB → N-SUB fan-out bench using embedded media driver +
//! IPC channel (shared-memory log buffer, the fastest Aeron transport).
//! Reports `delivered msg/s` like the kevy / ZeroMQ / Zenoh benches.
//!
//! IPC is Aeron's "in-host" mode — equivalent in fairness to:
//!   - kevy / valkey / redis: TCP loopback (forced by RESP)
//!   - zmq: tcp://127.0.0.1
//!   - zenoh: peer mode (TCP-loopback transports)
//! Aeron IPC is the upper bound — it shows what a dedicated messaging
//! library hits when it can skip the kernel network stack entirely.
//!
//! Build: cargo build --release
//! Run:   SUBS=50 MSGS=200000 SIZE=16 ./target/release/aeron_pubsub

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rusteron_client::*;
use rusteron_media_driver::{AeronDriverContext, AeronDriver};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subs_cnt: usize = env::var("SUBS").ok().and_then(|s| s.parse().ok()).unwrap_or(50);
    let msgs_cnt: usize = env::var("MSGS").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let size: usize = env::var("SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(16);

    // Embedded media driver — dedicated temp aeron-dir so we don't
    // collide with any system driver. After start() the caller MUST poll
    // `main_do_work()` continuously so the conductor can service
    // client requests; spawn a thread for that.
    let driver_ctx = AeronDriverContext::new()?;
    let driver = AeronDriver::new(&driver_ctx)?;
    driver.start(true)?;
    let aeron_dir = driver_ctx.get_dir().to_string();
    println!("driver started at dir={aeron_dir:?}");
    let driver_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let driver_running_t = Arc::clone(&driver_running);
    let driver_thread_handle = {
        let drv = driver.clone();
        thread::spawn(move || {
            while driver_running_t.load(Ordering::Acquire) {
                if let Ok(work) = drv.main_do_work() {
                    drv.main_idle_strategy(work);
                }
            }
        })
    };

    // Aeron client.
    let ctx = AeronContext::new()?;
    ctx.set_dir(&aeron_dir.into_c_string())?;
    let aeron = Aeron::new(&ctx)?;
    aeron.start()?;

    let stream_id = 1001;
    let channel = AERON_IPC_STREAM; // "aeron:ipc"

    // Publication.
    let publisher = aeron
        .async_add_publication(channel, stream_id)?
        .poll_blocking(Duration::from_secs(5))?;

    // Subscribers (each its own subscription, polled in its own thread).
    let delivered = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(subs_cnt);
    for _ in 0..subs_cnt {
        let sub = aeron
            .async_add_subscription(
                channel,
                stream_id,
                Handlers::no_available_image_handler(),
                Handlers::no_unavailable_image_handler(),
            )?
            .poll_blocking(Duration::from_secs(5))?;
        let delivered = Arc::clone(&delivered);
        let ready = Arc::clone(&ready);
        let h = thread::spawn(move || {
            struct H(Arc<AtomicUsize>);
            impl AeronFragmentHandlerCallback for H {
                fn handle_aeron_fragment_handler(&mut self, _b: &[u8], _h: AeronHeader) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
            let local_count = Arc::new(AtomicUsize::new(0));
            let cb = Handler::leak(H(Arc::clone(&local_count)));
            ready.fetch_add(1, Ordering::Relaxed);
            let target = msgs_cnt;
            while local_count.load(Ordering::Relaxed) < target {
                let _ = sub.poll(Some(&cb), 1024);
            }
            delivered.fetch_add(local_count.load(Ordering::Relaxed), Ordering::Relaxed);
            drop(cb); // free the leaked handler
        });
        handles.push(h);
    }

    // Wait for all subscriber images to bind (the publisher needs them to
    // accept its first offers without backpressure).
    while ready.load(Ordering::Relaxed) < subs_cnt {
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(200));

    let payload = vec![b'x'; size];
    let t0 = Instant::now();
    for _ in 0..msgs_cnt {
        // offer() returns positive position on success, negative on backpressure.
        // Spin on backpressure — pure throughput mode.
        loop {
            let p = publisher.offer(
                &payload[..],
                Handlers::no_reserved_value_supplier_handler(),
            );
            if p > 0 {
                break;
            }
            // backpressure / not-connected; small spin
        }
    }
    let pub_elapsed = t0.elapsed();

    for h in handles {
        let _ = h.join();
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
        "aeron-ipc subs={subs_cnt} msgs={msgs_cnt} size={size}B \
         delivered={delivered_rate} msg/s publishes={publish_rate}/s \
         elapsed={:.3}s pub_elapsed={:.3}s delivered_total={delivered_total}",
        total_elapsed.as_secs_f64(),
        pub_elapsed.as_secs_f64()
    );

    driver_running.store(false, Ordering::Release);
    let _ = driver_thread_handle.join();
    Ok(())
}
