//! Pub/sub exports. The wasm target has no threads, so delivery is a
//! polling drain: subscriptions queue frames inside the engine and the
//! host pulls them with [`kevy_poll_events`] on its own cadence
//! (typically a microtask right after each publish, plus the timer that
//! also drives the TTL tick).
//!
//! ```
//! use kevy_wasm::abi_core::*;
//! use kevy_wasm::abi_pubsub::*;
//! let h = kevy_open(0);
//! // SAFETY: each pair points at that many readable bytes for the call.
//! unsafe {
//!     kevy_subscribe(h, b"news".as_ptr(), 4);
//!     assert_eq!(kevy_publish(h, b"news".as_ptr(), 4, b"hi".as_ptr(), 2), 1);
//! }
//! assert_eq!(kevy_poll_events(h), 1); // the host drains on its own cadence
//! assert_eq!(kevy_poll_events(h), 0);
//! kevy_close(h);
//! ```

use crate::{BAD_HANDLE, arg, with};
use kevy_embedded::PubsubEvent;

/// Packed event kind: a direct channel message.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_subscribe(h, b"news".as_ptr(), 4);
///     kevy_publish(h, b"news".as_ptr(), 4, b"hi".as_ptr(), 2);
/// }
/// kevy_poll_events(h);
/// assert_eq!(out(h)[0], EVENT_MESSAGE); // each event starts with its kind
/// kevy_close(h);
/// ```
pub const EVENT_MESSAGE: u8 = 1;
/// Packed event kind: a pattern-subscription match.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_psubscribe(h, b"room:*".as_ptr(), 6);
///     kevy_publish(h, b"room:1".as_ptr(), 6, b"hi".as_ptr(), 2);
/// }
/// kevy_poll_events(h);
/// assert_eq!(out(h)[0], EVENT_PMESSAGE);
/// kevy_close(h);
/// ```
pub const EVENT_PMESSAGE: u8 = 2;

/// `SUBSCRIBE channel`. Returns a per-instance subscription id (`0` on a
/// bad handle). Each id has its own delivery queue — two subscriptions
/// on the same channel each see every message.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     let (a, b) = (kevy_subscribe(h, b"c".as_ptr(), 1), kevy_subscribe(h, b"c".as_ptr(), 1));
///     assert!(a != 0 && a != b);
///     assert_eq!(kevy_publish(h, b"c".as_ptr(), 1, b"x".as_ptr(), 1), 2); // both queues
///     assert_eq!(kevy_subscribe(0, b"c".as_ptr(), 1), 0); // bad handle
/// }
/// assert_eq!(kevy_poll_events(h), 2);
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
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
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     assert_ne!(kevy_psubscribe(h, b"user:?".as_ptr(), 6), 0);
///     assert_eq!(kevy_publish(h, b"user:7".as_ptr(), 6, b"x".as_ptr(), 1), 1);
///     assert_eq!(kevy_publish(h, b"user:42".as_ptr(), 7, b"x".as_ptr(), 1), 0); // `?` is one byte
/// }
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
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
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// let sub = unsafe { kevy_subscribe(h, b"c".as_ptr(), 1) };
/// assert_eq!(kevy_unsubscribe(h, sub), 0);
/// assert_eq!(kevy_unsubscribe(h, sub), -2); // already dropped
/// // SAFETY: as above.
/// assert_eq!(unsafe { kevy_publish(h, b"c".as_ptr(), 1, b"x".as_ptr(), 1) }, 0);
/// kevy_close(h);
/// ```
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
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// let (h, other) = (kevy_open(0), kevy_open(0));
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_subscribe(other, b"c".as_ptr(), 1);
///     // subscribers in another instance are not reached
///     assert_eq!(kevy_publish(h, b"c".as_ptr(), 1, b"x".as_ptr(), 1), 0);
///     kevy_subscribe(h, b"c".as_ptr(), 1);
///     assert_eq!(kevy_publish(h, b"c".as_ptr(), 1, b"x".as_ptr(), 1), 1);
/// }
/// kevy_close(h);
/// kevy_close(other);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
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
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_pubsub::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// let sub = unsafe {
///     let sub = kevy_subscribe(h, b"news".as_ptr(), 4);
///     kevy_publish(h, b"news".as_ptr(), 4, b"hi".as_ptr(), 2);
///     sub
/// };
/// assert_eq!(kevy_poll_events(h), 1);
/// let seg = |b: &[u8]| [&(b.len() as u32).to_le_bytes()[..], b].concat();
/// let want = [&[EVENT_MESSAGE][..], &sub.to_le_bytes(), &seg(b""), &seg(b"news"), &seg(b"hi")].concat();
/// assert_eq!(out(h), want); // kind, subscription id, pattern, channel, payload
/// kevy_close(h);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_poll_events(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| {
        inst.out.clear();
        let crate::Instance { subs, out, .. } = inst;
        let mut count = 0i32;
        for (id, sub) in subs.iter() {
            while let Ok(Some(frame)) = sub.try_recv() {
                match frame {
                    PubsubEvent::Message { channel, payload } => {
                        pack_event(out, EVENT_MESSAGE, *id, &[], &channel, &payload);
                        count += 1;
                    }
                    PubsubEvent::Pmessage { pattern, channel, payload } => {
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
