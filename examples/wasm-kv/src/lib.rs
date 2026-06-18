//! kevy-embedded as an in-process KV cache inside WebAssembly.
//!
//! "Best within the constraints": a browser / WASI wasm module has **no
//! threads** and (in the browser) **no filesystem**, so we run kevy-embedded in
//! its pure in-memory mode (with the manual TTL reaper, since there's no thread
//! to spawn). Same engine, same API as on a server — adapted to what the
//! runtime offers.
//!
//! Build (node-loadable):  `wasm-pack build --target nodejs` → `node run.cjs`
//!
//! ## Clock
//!
//! `wasm32-unknown-unknown` has no `Instant`/`SystemTime` (calling them traps),
//! so the host feeds time. Call [`KvCache::set_clock`] with `Date.now()` before
//! TTL-sensitive operations and once per [`KvCache::tick`]; kevy then drives
//! expiry off that host clock. (On native targets the OS clock is used directly
//! — no feeding needed.) This is what makes `set_with_ttl` / `pttl` / `del` /
//! the reaper work on wasm — earlier they panicked because the store read
//! `Instant::now()`.

use std::time::Duration;

use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};
use wasm_bindgen::prelude::*;

/// An in-process cache backed by kevy-embedded, usable from JS.
#[wasm_bindgen]
pub struct KvCache {
    store: Store,
}

#[wasm_bindgen]
impl KvCache {
    /// Open an in-memory cache. Manual TTL reaper (the browser has no thread to
    /// spawn a background reaper on).
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<KvCache, JsError> {
        let store = Store::open(Config::default().with_ttl_reaper_manual())
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(KvCache { store })
    }

    /// Feed kevy's clocks. Pass `Date.now()` (Unix-epoch millis). Drives both
    /// the monotonic deadline clock (TTL) and the wall clock (XADD/EXPIREAT).
    /// Call before TTL-sensitive ops and once per [`Self::tick`].
    pub fn set_clock(&self, now_ms: f64) {
        let ms = now_ms.max(0.0) as u64;
        set_clock_ns(ms.saturating_mul(1_000_000));
        set_wall_clock_ms(ms);
    }

    /// `SET key value`.
    pub fn set(&self, key: &str, value: &str) -> Result<(), JsError> {
        self.store
            .set(key.as_bytes(), value.as_bytes())
            .map(|_| ())
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// `SET key value PX ttl_ms` — value with an expiry. Feed [`Self::set_clock`]
    /// first so the deadline anchors to the host clock.
    pub fn set_with_ttl(&self, key: &str, value: &str, ttl_ms: u32) -> Result<(), JsError> {
        self.store
            .set_with_ttl(key.as_bytes(), value.as_bytes(), Duration::from_millis(ttl_ms as u64))
            .map(|_| ())
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// `GET key`. Returns `undefined` (JS) when absent or expired.
    pub fn get(&self, key: &str) -> Result<Option<String>, JsError> {
        let v = self
            .store
            .get(key.as_bytes())
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(v.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// `PTTL key`. ms remaining, `-2` no key, `-1` no TTL.
    pub fn pttl(&self, key: &str) -> i64 {
        self.store.ttl_ms(key.as_bytes())
    }

    /// `DEL key` — returns 1 if the key was present, else 0. (This is the call
    /// that used to trap on wasm: `del` reaps before removing, and reaping read
    /// `Instant::now()`.)
    pub fn del(&self, key: &str) -> Result<usize, JsError> {
        self.store
            .del(&[key.as_bytes()])
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Run one manual TTL-reaper sweep (call ~10×/s in a real app). Returns the
    /// number of keys it expired. Feed [`Self::set_clock`] first.
    pub fn tick(&self) -> u32 {
        self.store.tick().expired
    }

    /// `DBSIZE` — live key count.
    pub fn size(&self) -> usize {
        self.store.dbsize()
    }
}

impl Default for KvCache {
    fn default() -> Self {
        Self::new().expect("in-memory KvCache open is infallible")
    }
}
