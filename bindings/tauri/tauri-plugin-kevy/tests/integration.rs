//! Headless integration test: the plugin registers into a real Tauri app and
//! opens its store, under the `MockRuntime` (no webview, no bundler, no GUI).
//!
//! Building a `mock_builder().plugin(init())` app to completion is a genuine
//! assertion: Tauri runs the plugin's `setup` hook, which opens the embedded
//! `Store` and `app.manage`s it. A failure there (bad store open, a
//! `generate_handler!` arity mismatch) aborts the build. The command *behavior*
//! is covered against a live store in `src/logic_tests.rs`; the full IPC +
//! permission (ACL) path — which needs the plugin permission manifest that only
//! an app's `tauri-build` assembles — is exercised by the example app under
//! `bindings/tauri/example`.

use tauri::test::{mock_builder, mock_context, noop_assets};

#[test]
fn plugin_registers_and_opens_in_memory_store() {
    let app = mock_builder()
        .plugin(tauri_plugin_kevy::init())
        .build(mock_context(noop_assets()))
        .expect("plugin setup ran + in-memory store opened + app built");
    drop(app);
}

#[test]
fn plugin_builds_with_persistent_store() {
    let dir = std::env::temp_dir().join(format!("kevy-tauri-it-{}", std::process::id()));
    let app = mock_builder()
        .plugin(tauri_plugin_kevy::Builder::new().path(&dir).build())
        .build(mock_context(noop_assets()))
        .expect("persistent store opens + app builds");
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}
