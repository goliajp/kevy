//! Pub/sub streaming: bridge kevy's in-process bus to the webview.
//!
//! A `subscribe` command opens a kevy [`Subscription`] and hands a background
//! poller thread a Tauri [`Channel`]. The poller blocks on the bus with a
//! bounded `recv_timeout` (so it can also notice the stop flag) and forwards
//! each frame to the webview as a [`PubsubMsg`]. This matches the C-ABI
//! contract's "pub/sub is polled, not callback" posture (see
//! `docs/client-contract.md` §5.1) while presenting a push-style `Channel` API
//! to JS — the poll happens on the Rust side, off the webview thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use kevy_embedded::{KevyError, PubsubFrame, Store};
use serde::Serialize;
use tauri::ipc::Channel;

/// How long the poller parks on the bus before re-checking its stop flag.
const POLL_TICK: Duration = Duration::from_millis(250);

/// A pub/sub frame delivered to the webview over the subscription [`Channel`].
///
/// Serialises as `{ "kind": "<tag>", … }`, mirroring the [`PubsubEvent`] shape
/// in `docs/client-contract.md` §4.8. Payload/channel/pattern bytes are JSON
/// number arrays (binary-safe).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PubsubMsg {
    /// `SUBSCRIBE` ack.
    Subscribe {
        /// Subscribed channel.
        channel: Vec<u8>,
        /// Total channels + patterns held after the op.
        count: usize,
    },
    /// `PSUBSCRIBE` ack.
    Psubscribe {
        /// Subscribed pattern.
        pattern: Vec<u8>,
        /// Total channels + patterns held after the op.
        count: usize,
    },
    /// `UNSUBSCRIBE` ack (`channel: null` = "all").
    Unsubscribe {
        /// Unsubscribed channel, or `null` for "all".
        channel: Option<Vec<u8>>,
        /// Total still held after the op.
        count: usize,
    },
    /// `PUNSUBSCRIBE` ack (`pattern: null` = "all").
    Punsubscribe {
        /// Unsubscribed pattern, or `null` for "all".
        pattern: Option<Vec<u8>>,
        /// Total still held after the op.
        count: usize,
    },
    /// A message delivered on a directly subscribed channel.
    Message {
        /// Channel the publish targeted.
        channel: Vec<u8>,
        /// Payload bytes.
        payload: Vec<u8>,
    },
    /// A message delivered via a matching pattern subscription.
    Pmessage {
        /// The pattern that matched.
        pattern: Vec<u8>,
        /// Channel the publish targeted.
        channel: Vec<u8>,
        /// Payload bytes.
        payload: Vec<u8>,
    },
}

impl From<PubsubFrame> for PubsubMsg {
    fn from(f: PubsubFrame) -> Self {
        match f {
            PubsubFrame::Subscribe { channel, count } => PubsubMsg::Subscribe { channel, count },
            PubsubFrame::Psubscribe { pattern, count } => PubsubMsg::Psubscribe { pattern, count },
            PubsubFrame::Unsubscribe { channel, count } => PubsubMsg::Unsubscribe { channel, count },
            PubsubFrame::Punsubscribe { pattern, count } => {
                PubsubMsg::Punsubscribe { pattern, count }
            }
            PubsubFrame::Message { channel, payload } => PubsubMsg::Message { channel, payload },
            PubsubFrame::Pmessage { pattern, channel, payload } => {
                PubsubMsg::Pmessage { pattern, channel, payload }
            }
        }
    }
}

/// Open a subscription over `channels` + `patterns` and spawn the poller that
/// forwards frames to `sink`. Returns the shared stop flag and the thread
/// handle for the state registry to own.
pub fn spawn(
    store: &Store,
    channels: Vec<Vec<u8>>,
    patterns: Vec<Vec<u8>>,
    sink: Channel<PubsubMsg>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let chan_refs: Vec<&[u8]> = channels.iter().map(Vec::as_slice).collect();
    let mut sub = store.subscribe(&chan_refs);
    if !patterns.is_empty() {
        let pat_refs: Vec<&[u8]> = patterns.iter().map(Vec::as_slice).collect();
        sub.psubscribe(&pat_refs);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = std::thread::spawn(move || poll_loop(sub, &stop_thread, &sink));
    (stop, thread)
}

/// Drain the subscription into `sink` until stopped, the bus closes, or the
/// webview channel goes away.
fn poll_loop(sub: kevy_embedded::Subscription, stop: &AtomicBool, sink: &Channel<PubsubMsg>) {
    while !stop.load(Ordering::SeqCst) {
        match sub.recv_timeout(POLL_TICK) {
            Ok(frame) => {
                if sink.send(PubsubMsg::from(frame)).is_err() {
                    break; // webview gone — stop draining.
                }
            }
            Err(KevyError::TimedOut) => {}
            _ => break, // Closed (bus dropped) or any other terminal error.
        }
    }
}

#[cfg(test)]
#[path = "pubsub_tests.rs"]
mod tests;
