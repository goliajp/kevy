# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Tracks the kevy 7.0.0 engine. No API change in this package; the platform
packages carry the 7.0.0 engine. What changes underneath:

- **A killed process keeps every write that returned.** Appends used to
  wait in a user-space buffer until the next tick or fsync. On macOS the
  AOF is now written through a memory map; on Linux appends go to a small
  mapped staging file that the next open replays. Power loss is bounded by
  the fsync policy, as before.
- **Streams and geo run in the embedded engine.** The stream commands
  (`XADD`, `XRANGE`, `XREADGROUP`, `XACK`, `XCLAIM`, `XINFO`, …) and the
  `GEO*` commands answer through `cmd()`. A `BLOCK` read is refused with
  `ERR the embedded engine cannot block; call without BLOCK`, and a
  multi-key read or a geo store across shards with `CROSSSLOT`.
- **`SET … NX|XX EX|PX` and the `EXPIRE` family each run as one
  operation.** Since 4.0.0 `SET NX EX` set the value and its TTL under two
  locks.
- **Writes made after a crash inside a transaction survive the next
  restart** (since 4.0.0), and **a hash field's own TTL survives a
  background AOF rewrite** (since 3.0.0).
- **`COPY` copies a key of any type.** Since 2.0.13 only strings were
  copied; other types answered `WRONGTYPE`.
- **An `MSET` keeps each shard's pairs whole across a crash.** The log and
  the change feed show one `MSET` frame per shard instead of one `SET` per
  key.
- **Opening is about 2.8× faster and closing returns at once.**

Downgrading: 6.4 does not understand the mapped log's zero tail or the
staging file, so after a process has been killed on 7.0, open and close the
store cleanly with 7.0 before going back. 6.4 also opens a directory 7.0
wrote without its indexes, views and tables. See
[the upgrade guide](https://github.com/goliajp/kevy/blob/develop/docs/upgrading-6.4-to-7.0.md).
