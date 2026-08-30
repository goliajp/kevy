//! Pub/sub exports. The wasm target has no threads, so delivery is a
//! polling drain: subscriptions queue frames inside the engine and the
//! host pulls them with [`kevy_poll_events`] on its own cadence
//! (typically a microtask right after each publish, plus the timer that
//! also drives the TTL tick).

use crate::{BAD_HANDLE, arg, with};
use kevy_embedded::PubsubFrame;

/// Packed event kind: a direct channel message.
pub const EVENT_MESSAGE: u8 = 1;
/// Packed event kind: a pattern-subscription match.
pub const EVENT_PMESSAGE: u8 = 2;

/// `SUBSCRIBE channel`. Returns a per-instance subscription id (`0` on a
/// bad handle). Each id has its own delivery queue — two subscriptions
/// on the same channel each see every message.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_subscribe(h: u32, cp: *const u8, cl: u32) -> u32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let channel = unsafe { arg(cp, cl) };
    with(h, 0, |inst| {
        let sub = inst.store.subscribe(&[channel]);
        let id = inst.next_sub;
        inst.next_sub += 1;
        inst.subs.insert(id, sub);
        id
    })
}

/// `PSUBSCRIBE pattern` (Redis glob syntax: `*`, `?`, `[abc]`). Returns
/// a subscription id like [`kevy_subscribe`].
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_psubscribe(h: u32, pp: *const u8, pl: u32) -> u32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let pattern = unsafe { arg(pp, pl) };
    with(h, 0, |inst| {
        let sub = inst.store.psubscribe(&[pattern]);
        let id = inst.next_sub;
        inst.next_sub += 1;
        inst.subs.insert(id, sub);
        id
    })
}

/// Drop subscription `sub` (unsubscribes and discards queued frames).
/// Returns 0, or -2 when the handle or the subscription id is unknown.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_unsubscribe(h: u32, sub: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| match inst.subs.remove(&sub) {
        Some(_) => crate::OK,
        None => BAD_HANDLE,
    })
}

/// `PUBLISH channel payload`. Returns the number of subscriptions in
/// **this instance** the message reached. Cross-context fan-out (other
/// tabs, workers) is the host bridge's job — see the loader's
/// BroadcastChannel bridge.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_publish(
    h: u32,
    cp: *const u8,
    cl: u32,
    pp: *const u8,
    pl: u32,
) -> i32 {
    // SAFETY: loader-staged argument buffers, live for this call.
    let (channel, payload) = unsafe { (arg(cp, cl), arg(pp, pl)) };
    with(h, BAD_HANDLE, |inst| inst.store.publish(channel, payload) as i32)
}

/// Drain every queued message across this instance's subscriptions into
/// the result buffer. Returns the event count.
///
/// Each event is packed flat as: `u8` kind ([`EVENT_MESSAGE`] /
/// [`EVENT_PMESSAGE`]), `u32` subscription id, then three
/// length-prefixed byte segments (`u32` little-endian length + bytes):
/// pattern (empty for direct messages), channel, payload.
/// Subscribe/unsubscribe acknowledgements are consumed silently — the
/// polling model has no use for them.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_poll_events(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| {
        inst.out.clear();
        let crate::Instance { subs, out, .. } = inst;
        let mut count = 0i32;
        for (id, sub) in subs.iter() {
            while let Ok(Some(frame)) = sub.try_recv() {
                match frame {
                    PubsubFrame::Message { channel, payload } => {
                        pack_event(out, EVENT_MESSAGE, *id, &[], &channel, &payload);
                        count += 1;
                    }
                    PubsubFrame::Pmessage { pattern, channel, payload } => {
                        pack_event(out, EVENT_PMESSAGE, *id, &pattern, &channel, &payload);
                        count += 1;
                    }
                    _ => {}
                }
            }
        }
        count
    })
}

/// Append one packed event (see [`kevy_poll_events`] for the layout).
fn pack_event(out: &mut Vec<u8>, kind: u8, sub: u32, a: &[u8], b: &[u8], c: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&sub.to_le_bytes());
    for seg in [a, b, c] {
        out.extend_from_slice(&(seg.len() as u32).to_le_bytes());
        out.extend_from_slice(seg);
    }
}
