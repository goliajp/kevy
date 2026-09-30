# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

For `tauri-plugin-kevy` 7.0.0. No API change in this package. What the
webview sees differently comes from the plugin and the engine it embeds:

- **The read-only error's `message` is the server's:** `READONLY You can't
  write against a read only replica.` The plugin used to say `READONLY the
  store is a read-only replica`. Its `kind` is still `ReadOnly`.
- **An error can arrive with `kind: "Other"`,** carrying the engine's
  message, for an engine error the plugin has no kind of its own for.
- **A pub/sub event of a kind the plugin does not know is no longer
  forwarded** to the subscription callback.
- **Streams and geo run in the embedded engine.** The stream commands and
  the `GEO*` commands answer through `kevy.cmd()`. A `BLOCK` read is
  refused with `ERR the embedded engine cannot block; call without BLOCK`,
  and a multi-key read or a geo store across shards with `CROSSSLOT`.
- **`COPY` copies a key of any type** (since 2.0.13 only strings), and
  **`SET … NX|XX EX|PX` and the `EXPIRE` family each run as one
  operation** (since 4.0.0 `SET NX EX` took two locks).

A persistent store's durability and downgrade notes are in the plugin's
[changelog](https://github.com/goliajp/kevy/blob/develop/bindings/tauri/tauri-plugin-kevy/CHANGELOG.md).
