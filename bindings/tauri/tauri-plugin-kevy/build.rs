//! Autogenerate this plugin's Access-Control-List (ACL) permissions.
//!
//! For every command name in `COMMANDS`, `tauri_plugin::Builder` emits an
//! `allow-<cmd>` and a `deny-<cmd>` permission plus the JSON schema the app's
//! capability files validate against. The `permissions/default.toml` set (which
//! we author by hand) references those `allow-*` identifiers. Keep this list in
//! sync with the `#[tauri::command]`s registered in `src/lib.rs`.

const COMMANDS: &[&str] = &[
    "cmd",
    "get",
    "set",
    "del",
    "exists",
    "incr",
    "ping",
    "dbsize",
    "flushall",
    "publish",
    "subscribe",
    "unsubscribe",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
