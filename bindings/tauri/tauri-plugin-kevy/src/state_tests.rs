//! Unit tests for the subscription registry lifecycle.

use super::KevyState;
use kevy_embedded::{Config, Store};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Spawn a stand-in poller thread that spins until its stop flag is set.
fn dummy_poller() -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let t = std::thread::spawn(move || {
        while !s.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    (stop, t)
}

#[test]
fn register_then_unregister_is_idempotent() {
    let store = Store::open(Config::default()).unwrap();
    let state = KevyState::new(store);

    let (stop, thread) = dummy_poller();
    let id = state.register(stop.clone(), thread);
    assert!(id >= 1);

    // First unregister stops + joins the poller and reports it existed.
    assert!(state.unregister(id));
    assert!(stop.load(Ordering::SeqCst), "poller stop flag set on unregister");

    // Second unregister of the same id is a no-op.
    assert!(!state.unregister(id));
}

#[test]
fn drop_stops_all_pollers() {
    let store = Store::open(Config::default()).unwrap();
    let (stop, thread) = dummy_poller();
    let flag = stop.clone();
    {
        let state = KevyState::new(store);
        state.register(stop, thread);
        // state drops here.
    }
    assert!(flag.load(Ordering::SeqCst), "drop signalled the poller to stop");
}
