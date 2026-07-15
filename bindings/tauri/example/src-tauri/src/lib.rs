//! The example app's Tauri entry point.
//!
//! It registers `tauri-plugin-kevy` with a default in-memory store, so the
//! webview can `invoke('plugin:kevy|…')` against one shared kevy engine living
//! in this Rust process. Swap `init()` for `Builder::new().path("…")` to make
//! it persistent (snapshot + AOF), or `.store(existing)` to share a store this
//! Rust code also holds.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_kevy::init())
        .run(tauri::generate_context!())
        .expect("error while running the kevy Tauri example");
}
