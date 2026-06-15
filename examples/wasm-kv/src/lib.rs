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
//! ⚠️ **Known limitation (tracked):** TTL operations (`set_with_ttl`,
//! `PEXPIRE`, `PTTL`, the reaper `tick`) are NOT exposed here because
//! kevy-store's clock reads `std::time::Instant::now()`, which **panics on
//! `wasm32-unknown-unknown`** (that target has no monotonic clock). The
//! non-expiring core (set/get/del/dbsize) works fully. Making TTL work on wasm
//! needs an `Instant`→ns clock port with a host-fed time source — see the
//! kevy roadmap. (CI only *compile*-checks wasm; this is caught by *running*.)

use kevy_embedded::{Config, Store};
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

    /// `SET key value`.
    pub fn set(&self, key: &str, value: &str) -> Result<(), JsError> {
        self.store
            .set(key.as_bytes(), value.as_bytes())
            .map(|_| ())
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// `GET key`. Returns `undefined` (JS) when absent.
    pub fn get(&self, key: &str) -> Result<Option<String>, JsError> {
        let v = self
            .store
            .get(key.as_bytes())
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(v.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
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
