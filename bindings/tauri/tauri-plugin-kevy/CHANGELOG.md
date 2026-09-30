# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Tracks the kevy 7.0.0 engine. API changes in this crate:

- **`Error` gains `Other(String)`** (its `kind()` is `"Other"`), for a
  `KevyError` kind newer than the plugin; a `match` on `Error` needs an arm
  for it.
- **The pub/sub ack's `count` is an `i64`** (was `usize`) on
  `PubsubMsg::Subscribe`, `Psubscribe`, `Unsubscribe` and `Punsubscribe`.
- **`PubsubMsg` converts from the engine's `PubsubEvent` with
  `TryFrom`,** replacing `From<PubsubFrame>`; an event kind the plugin has
  no webview shape for is given back and not forwarded.
- **`Error::ReadOnly` displays the server's text,** `READONLY You can't
  write against a read only replica.`, instead of `READONLY the store is a
  read-only replica`.
- **`Store` and `Config`, re-exported from `kevy-embedded`, follow its 7.0
  API.** The old and new signatures are listed in
  [docs/rust-api-7.0.md](https://github.com/goliajp/kevy/blob/develop/docs/rust-api-7.0.md).

What changes underneath, in the embedded store:

- **A killed app keeps every write that returned.** On macOS and iOS the
  AOF is written through a memory map; elsewhere appends go to a small
  mapped staging file that the next open replays. Power loss is bounded by
  the fsync policy, as before.
- **Streams and geo commands run through `cmd`,** except a `BLOCK` read,
  which is refused, and a multi-key read or a geo store across shards,
  which answers `CROSSSLOT`.
- **Writes made after a crash inside a transaction survive the next
  restart** (since 4.0.0), **a hash field's own TTL survives a background
  AOF rewrite** (since 3.0.0), and **`COPY` copies a key of any type**
  (since 2.0.13).
- **Opening is about 2.8× faster and closing returns at once.**

Downgrading a persistent store: 6.4 does not understand the mapped log's
zero tail or the staging file, so after the app has been killed on 7.0,
open and close the store cleanly with 7.0 before going back. 6.4 also
opens a directory 7.0 wrote without its indexes, views and tables. See
[the upgrade guide](https://github.com/goliajp/kevy/blob/develop/docs/upgrading-6.4-to-7.0.md).
