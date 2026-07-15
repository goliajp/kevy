//! Unit tests for the pub/sub bridge: a real embedded store publish must reach
//! a Tauri `Channel` through the poller thread `spawn` starts.

use super::{spawn, PubsubMsg};
use kevy_embedded::{Config, Store};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;

/// A Channel whose handler records every delivered message as a JSON value.
fn recording_channel() -> (Channel<PubsubMsg>, Arc<Mutex<Vec<serde_json::Value>>>) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let sink2 = sink.clone();
    let ch = Channel::new(move |body| {
        let v: serde_json::Value = body.deserialize().expect("deserialize frame");
        sink2.lock().unwrap().push(v);
        Ok(())
    });
    (ch, sink)
}

/// Block until `sink` holds a message matching `pred`, or time out.
fn wait_for(
    sink: &Arc<Mutex<Vec<serde_json::Value>>>,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(v) = sink.lock().unwrap().iter().find(|v| pred(v)).cloned() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for pubsub frame");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn published_message_reaches_the_channel() {
    let store = Store::open(Config::default()).unwrap();
    let (ch, sink) = recording_channel();
    let (stop, thread) = spawn(&store, vec![b"room".to_vec()], vec![], ch);

    // The subscribe ack arrives first, then the message.
    wait_for(&sink, |v| v["kind"] == "subscribe" && v["count"] == 1);
    assert_eq!(store.publish(b"room", b"hello"), 1);
    let msg = wait_for(&sink, |v| v["kind"] == "message");
    let payload: Vec<u8> = serde_json::from_value(msg["payload"].clone()).unwrap();
    assert_eq!(payload, b"hello".to_vec());
    let channel: Vec<u8> = serde_json::from_value(msg["channel"].clone()).unwrap();
    assert_eq!(channel, b"room".to_vec());

    // Stopping the poller ends the thread within a poll tick.
    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap();
}

#[test]
fn pattern_subscribe_delivers_pmessage() {
    let store = Store::open(Config::default()).unwrap();
    let (ch, sink) = recording_channel();
    let (stop, thread) = spawn(&store, vec![], vec![b"ro*".to_vec()], ch);

    wait_for(&sink, |v| v["kind"] == "psubscribe");
    store.publish(b"room", b"x");
    let msg = wait_for(&sink, |v| v["kind"] == "pmessage");
    let pattern: Vec<u8> = serde_json::from_value(msg["pattern"].clone()).unwrap();
    assert_eq!(pattern, b"ro*".to_vec());

    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap();
}
