//! The managed plugin state: the shared [`Store`] plus a registry of live
//! pub/sub poller threads.
//!
//! One `KevyState` is [`app.manage`](tauri::Manager::manage)d at plugin init;
//! every `#[tauri::command]` reaches the same `Store` through it — that is the
//! "one shared persistent store in the backend" the plugin exists to provide.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use kevy_embedded::Store;

/// A running pub/sub poller: the thread draining a subscription plus the flag
/// that tells it to stop. `stop` is shared with the thread; setting it makes
/// the thread break out of its bounded `recv_timeout` loop.
struct SubHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SubHandle {
    /// Signal the poller to stop and join it (bounded — the thread wakes at
    /// most one `recv_timeout` tick after the flag is set).
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The Tauri-managed state for this plugin.
pub struct KevyState {
    store: Store,
    subs: Mutex<HashMap<u64, SubHandle>>,
    next_id: AtomicU64,
}

impl KevyState {
    /// Wrap an already-open [`Store`].
    pub fn new(store: Store) -> Self {
        Self {
            store,
            subs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The shared embedded store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Register a poller thread + its stop flag, returning the subscription id
    /// the webview uses to `unsubscribe` later.
    pub fn register(&self, stop: Arc<AtomicBool>, thread: JoinHandle<()>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut subs = self.subs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        subs.insert(id, SubHandle { stop, thread: Some(thread) });
        id
    }

    /// Stop and join the poller with `id`. Returns whether such a
    /// subscription existed (idempotent — a repeat call returns `false`).
    pub fn unregister(&self, id: u64) -> bool {
        let handle = {
            let mut subs = self.subs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            subs.remove(&id)
        };
        // Join OUTSIDE the registry lock so a poller mid-emit can't deadlock us.
        match handle {
            Some(mut h) => {
                h.shutdown();
                true
            }
            None => false,
        }
    }
}

impl Drop for KevyState {
    /// Tear down every live poller when the plugin state drops (app exit).
    fn drop(&mut self) {
        let mut subs = self.subs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, mut h) in subs.drain() {
            h.shutdown();
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
