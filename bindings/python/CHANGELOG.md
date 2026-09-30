# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Tracks the kevy 7.0.0 engine. One change in this package:

- **`ReadOnlyError`'s default message is the server's, word for word:**
  `READONLY You can't write against a read only replica.` It lacked the
  closing period. Code that matches the `READONLY` prefix or catches
  `ReadOnlyError` needs nothing.

The package is a network client with an optional embedded backend. Against
a server (`kevy://`, `redis://`, `tcp://`), what you observe is the
server's version; its changes are in the repository changelog. The
embedded backend (`mem://`, `file://`, `open_mem`, `open_persistent`) runs
whichever `libkevy_ffi` you built; with one built from 7.0.0:

- **A killed process keeps every write that returned.** On macOS the AOF
  is written through a memory map; elsewhere appends go to a small mapped
  staging file that the next open replays. Power loss is bounded by the
  fsync policy, as before.
- **Streams and geo run in the embedded engine.** The stream commands and
  the `GEO*` commands answer through `do()` and `DB.cmd()`. A `BLOCK` read
  is refused with `ERR the embedded engine cannot block; call without
  BLOCK`, and a multi-key read or a geo store across shards with
  `CROSSSLOT`.
- **`SET … NX|XX EX|PX` and the `EXPIRE` family each run as one
  operation** (since 4.0.0 `SET NX EX` took two locks), **writes made
  after a crash inside a transaction survive the next restart** (since
  4.0.0), and **a hash field's own TTL survives a background AOF rewrite**
  (since 3.0.0).
- **`COPY` copies a key of any type** (since 2.0.13 only strings).

Downgrading a `file://` store to a 6.4 library: 6.4 does not understand the
mapped log's zero tail or the staging file, so after a process has been
killed on 7.0, open and close the store cleanly with 7.0 first. 6.4 also
opens a directory 7.0 wrote without its indexes, views and tables. See
[the upgrade guide](https://github.com/goliajp/kevy/blob/develop/docs/upgrading-6.4-to-7.0.md).
