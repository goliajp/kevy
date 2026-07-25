//! The `#[tauri::command]` surface the webview invokes as
//! `invoke('plugin:kevy|<name>', …)`.
//!
//! Each command is a thin wrapper: it pulls the shared [`Store`] out of the
//! managed [`KevyState`] and delegates to [`crate::logic`], which holds the
//! actual behavior (and its tests). Commands are synchronous — embedded ops are
//! in-process (microsecond-scale mutex work), matching the engine's blocking
//! nature. `subscribe` is the exception: it hands a background thread a
//! [`Channel`] and returns an id immediately.
//!
//! Keys/values/channels cross the IPC boundary as `Vec<u8>` (JSON number
//! arrays) so all data is binary-safe; the guest binding packs `string`/
//! `Uint8Array` into that shape (see `guest-js/src/index.ts`). Tauri maps
//! camelCase JS arg keys to these snake_case params (`ttlMs` → `ttl_ms`).

use tauri::ipc::Channel;
use tauri::State;

use crate::error::{Error, Result};
use crate::logic;
use crate::pubsub::{self, PubsubMsg};
use crate::reply::JsonReply;
use crate::state::KevyState;

/// Raw command path: run any of kevy's ~184 verbs by argv and get the decoded
/// RESP reply. The escape hatch that keeps every engine capability reachable
/// from the webview without a per-verb command.
#[tauri::command]
pub fn cmd(argv: Vec<Vec<u8>>, state: State<'_, KevyState>) -> Result<JsonReply> {
    logic::raw_cmd(state.store(), &argv)
}

/// `GET key` — the scalar fast path. `null` when absent or expired.
#[tauri::command]
pub fn get(key: Vec<u8>, state: State<'_, KevyState>) -> Result<Option<Vec<u8>>> {
    logic::get(state.store(), &key)
}

/// `SET key value [PX ttl_ms]`. `ttl_ms` of `None`/`0` sets no expiry.
#[tauri::command]
pub fn set(
    key: Vec<u8>,
    value: Vec<u8>,
    ttl_ms: Option<u64>,
    state: State<'_, KevyState>,
) -> Result<()> {
    logic::set(state.store(), &key, &value, ttl_ms)
}

/// `DEL key [key …]` — number of keys actually removed.
#[tauri::command]
pub fn del(keys: Vec<Vec<u8>>, state: State<'_, KevyState>) -> Result<usize> {
    logic::del(state.store(), &keys)
}

/// `EXISTS key [key …]` — a repeated key counts each time (Redis semantics).
#[tauri::command]
pub fn exists(keys: Vec<Vec<u8>>, state: State<'_, KevyState>) -> Result<usize> {
    logic::exists(state.store(), &keys)
}

/// `INCR key` (or `INCRBY key by` when `by` is given). Post-increment value.
#[tauri::command]
pub fn incr(key: Vec<u8>, by: Option<i64>, state: State<'_, KevyState>) -> Result<i64> {
    logic::incr(state.store(), &key, by)
}

/// `PING` — always OK on the embedded backend.
#[tauri::command]
pub fn ping(_state: State<'_, KevyState>) -> Result<()> {
    Ok(())
}

/// `DBSIZE` — live keys at call time.
#[tauri::command]
pub fn dbsize(state: State<'_, KevyState>) -> Result<usize> {
    Ok(logic::dbsize(state.store()))
}

/// `FLUSHALL` — wipe the store.
#[tauri::command]
pub fn flushall(state: State<'_, KevyState>) -> Result<()> {
    logic::flushall(state.store())
}

/// `PUBLISH channel payload` — count of in-process subscribers reached.
#[tauri::command]
pub fn publish(channel: Vec<u8>, payload: Vec<u8>, state: State<'_, KevyState>) -> Result<usize> {
    Ok(logic::publish(state.store(), &channel, &payload))
}

/// Subscribe to `channels` (exact) and/or `patterns` (glob). Frames stream to
/// the webview over `on_event`; the returned id feeds `unsubscribe`.
#[tauri::command]
pub fn subscribe(
    channels: Vec<Vec<u8>>,
    patterns: Vec<Vec<u8>>,
    on_event: Channel<PubsubMsg>,
    state: State<'_, KevyState>,
) -> Result<u64> {
    if channels.is_empty() && patterns.is_empty() {
        return Err(Error::InvalidInput("subscribe needs a channel or pattern".into()));
    }
    let (stop, thread) = pubsub::spawn(state.store(), channels, patterns, on_event);
    Ok(state.register(stop, thread))
}

/// Stop the subscription poller with `id`. `true` if it existed.
#[tauri::command]
pub fn unsubscribe(id: u64, state: State<'_, KevyState>) -> Result<bool> {
    Ok(state.unregister(id))
}
