//! The store-backed logic behind each command, as plain functions over a
//! `&Store`.
//!
//! Keeping the substance here — argument handling, the TTL branch, error
//! mapping, RESP decoding, the empty-argv guard — separates it from the Tauri
//! transport (`State` extraction + the `#[tauri::command]` macro in
//! `commands.rs`). That split is what lets the suite exercise every command's
//! real behavior against a live embedded store headlessly, without standing up
//! an app + webview + ACL (see `logic_tests.rs`). The full IPC + permission
//! path is exercised by the example app.

use std::time::Duration;

use kevy_embedded::Store;

use crate::error::{Error, Result};
use crate::reply::{decode, JsonReply};

/// Borrow a `Vec<Vec<u8>>` as the `&[&[u8]]` the store APIs take.
fn as_refs(v: &[Vec<u8>]) -> Vec<&[u8]> {
    v.iter().map(Vec::as_slice).collect()
}

/// Run any verb by argv and decode the RESP reply. Empty argv is rejected.
pub fn raw_cmd(store: &Store, argv: &[Vec<u8>]) -> Result<JsonReply> {
    if argv.is_empty() {
        return Err(Error::InvalidInput("cmd needs at least a verb".into()));
    }
    let mut out = Vec::new();
    store.dispatch_argv(argv, &mut out);
    decode(&out)
}

/// `GET key`.
pub fn get(store: &Store, key: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(store.get(key)?)
}

/// `SET key value [PX ttl_ms]`; `ttl_ms` of `None`/`0` sets no expiry.
pub fn set(store: &Store, key: &[u8], value: &[u8], ttl_ms: Option<u64>) -> Result<()> {
    match ttl_ms {
        Some(ms) if ms > 0 => {
            store.set_with_ttl(key, value, Duration::from_millis(ms))?;
        }
        _ => {
            store.set(key, value)?;
        }
    }
    Ok(())
}

/// `DEL key [key …]`.
pub fn del(store: &Store, keys: &[Vec<u8>]) -> Result<usize> {
    Ok(store.del(&as_refs(keys))?)
}

/// `EXISTS key [key …]` (a repeated key counts each time).
pub fn exists(store: &Store, keys: &[Vec<u8>]) -> Result<usize> {
    Ok(store.exists(&as_refs(keys))?)
}

/// `INCR key` / `INCRBY key by`.
pub fn incr(store: &Store, key: &[u8], by: Option<i64>) -> Result<i64> {
    Ok(match by {
        Some(delta) => store.incr_by(key, delta)?,
        None => store.incr(key)?,
    })
}

/// `DBSIZE`.
pub fn dbsize(store: &Store) -> usize {
    store.dbsize()
}

/// `FLUSHALL`.
pub fn flushall(store: &Store) -> Result<()> {
    Ok(store.flushall()?)
}

/// `PUBLISH channel payload` — in-process subscribers reached.
pub fn publish(store: &Store, channel: &[u8], payload: &[u8]) -> usize {
    store.publish(channel, payload)
}

#[cfg(test)]
#[path = "logic_tests.rs"]
mod tests;
