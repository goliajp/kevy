//! # tauri-plugin-kevy
//!
//! Embed [kevy](https://github.com/goliajp/kevy) — the pure-Rust,
//! zero-dependency, Redis-compatible engine — directly in a **Tauri v2**
//! backend and drive it from the webview.
//!
//! The plugin holds one [`kevy_embedded::Store`] in Tauri's managed state and
//! exposes it to JS through `#[tauri::command]`s: a raw `cmd` argv path (every
//! kevy verb), typed key ops (`get`/`set`/`del`/`exists`/`incr`), and pub/sub
//! that streams to the webview over a Tauri [`Channel`](tauri::ipc::Channel).
//! One shared store lives in the Rust process — every webview call and every
//! Rust-side call see the same keyspace.
//!
//! ## Register
//!
//! ```ignore
//! // In your app's src-tauri/src/lib.rs (needs a tauri.conf.json — hence
//! // `ignore`; this is app-side illustration, not plugin code):
//! tauri::Builder::default()
//!     // In-memory (nothing survives the process):
//!     .plugin(tauri_plugin_kevy::init())
//!     .run(tauri::generate_context!())
//!     .expect("run tauri app");
//! ```
//!
//! Persistent, or sharing a store you already opened in Rust:
//!
//! ```no_run
//! use tauri_plugin_kevy::{Builder, Store, Config};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Snapshot + AOF under ./data, restart-safe:
//! let persistent = Builder::new().path("./data");
//!
//! // …or hand the plugin a Store your Rust code also holds a clone of:
//! let store = Store::open(Config::default())?;
//! let shared = Builder::new().store(store.clone());
//! # let _ = (persistent, shared, store);
//! # Ok(())
//! # }
//! ```
//!
//! ## Charter note
//!
//! kevy's core workspace is strictly 0-dependency. A Tauri plugin unavoidably
//! depends on the `tauri` crate, so **this crate is intentionally NOT a member
//! of the kevy workspace** — it sits at the honest ecosystem boundary, exactly
//! where a real Tauri app does. `kevy-embedded` (the engine) stays 0-dep.
#![forbid(unsafe_code)]

mod commands;
mod error;
mod logic;
mod pubsub;
mod reply;
mod state;

use std::path::PathBuf;

use tauri::{
    plugin::{Builder as PluginBuilder, TauriPlugin},
    Manager, Runtime,
};

pub use error::{Error, Result};
pub use pubsub::PubsubMsg;
pub use reply::JsonReply;
use state::KevyState;

// Re-export the engine surface so an app can build a Config / Store without a
// separate `kevy-embedded` dependency line.
pub use kevy_embedded::{Config, Store};

/// How the plugin obtains its [`Store`] at init time.
enum Source {
    /// Open a fresh in-memory store (nothing survives the process).
    InMemory,
    /// Open a persistent store rooted at this directory (snapshot + AOF).
    Path(PathBuf),
    /// Adopt a store the caller already opened (shared with Rust-side code).
    /// Boxed to keep the enum small (a `Store` is several `Arc`s wide).
    Existing(Box<Store>),
}

/// Builder for the kevy Tauri plugin.
///
/// Defaults to an in-memory store; call [`Self::path`] for persistence or
/// [`Self::store`] to share a store your Rust code also holds.
pub struct Builder {
    source: Source,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// A new builder backed by a fresh in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self { source: Source::InMemory }
    }

    /// Back the plugin with an in-memory store (the default).
    #[must_use]
    pub fn in_memory(mut self) -> Self {
        self.source = Source::InMemory;
        self
    }

    /// Back the plugin with a persistent store rooted at `dir`
    /// (snapshot + AOF; replayed on open, restart-safe).
    #[must_use]
    pub fn path(mut self, dir: impl Into<PathBuf>) -> Self {
        self.source = Source::Path(dir.into());
        self
    }

    /// Back the plugin with a [`Store`] the caller already opened. `Store` is
    /// `Clone` (a cheap `Arc` bump), so pass `store.clone()` to keep a handle
    /// for Rust-side access — both see the same keyspace and pub/sub bus.
    #[must_use]
    pub fn store(mut self, store: Store) -> Self {
        self.source = Source::Existing(Box::new(store));
        self
    }

    /// Build the Tauri plugin. The store is opened in the plugin `setup` hook;
    /// an open failure (e.g. persistence I/O) aborts app start with the error.
    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let source = self.source;
        PluginBuilder::new("kevy")
            .invoke_handler(tauri::generate_handler![
                commands::cmd,
                commands::get,
                commands::set,
                commands::del,
                commands::exists,
                commands::incr,
                commands::ping,
                commands::dbsize,
                commands::flushall,
                commands::publish,
                commands::subscribe,
                commands::unsubscribe,
            ])
            .setup(move |app, _api| {
                let store = open_source(source)?;
                app.manage(KevyState::new(store));
                Ok(())
            })
            .build()
    }
}

/// Open the store described by `source`, mapping any engine error into a
/// boxed error the plugin `setup` hook can propagate.
fn open_source(source: Source) -> std::result::Result<Store, Box<dyn std::error::Error>> {
    let store = match source {
        Source::InMemory => Store::open(Config::default())?,
        Source::Path(dir) => Store::open(Config::default().with_persist(dir))?,
        Source::Existing(store) => *store,
    };
    Ok(store)
}

/// Initialise the plugin with a default in-memory store. Shorthand for
/// `Builder::new().build()`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new().build()
}
