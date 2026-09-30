# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Tracks the kevy 7.0.0 engine. API changes in this door:

- **`KevyDB.mset(vararg pairs: Pair<String, ByteArray>)` is new.** It sets
  every pair in one native call; the pairs of one shard are set and logged
  together.
- **`KevyDB.mget` makes one native call** instead of running an `MGET`
  through the command path and parsing the RESP reply in Kotlin. The
  results are the same.
- **`KevyNative` gains `mget(db, packedKeys)` and `mset(db, packedPairs)`,**
  the JNI lanes behind the two methods above.

What changes underneath:

- **A killed app keeps every write that returned.** Appends used to wait
  in a user-space buffer until the next tick or fsync, so an app killed in
  between (a crash, the Android memory killer) lost them. On Android
  appends now go to a small mapped staging file (`aof-<i>.aof.stage`) that
  the next open replays. Power loss is bounded by the fsync policy, as
  before.
- **Streams and geo run in the embedded engine.** The stream commands and
  the `GEO*` commands answer through `cmd()`. A `BLOCK` read is refused
  with `ERR the embedded engine cannot block; call without BLOCK`, and a
  multi-key read or a geo store across shards with `CROSSSLOT`.
- **`SET … NX|XX EX|PX` and the `EXPIRE` family each run as one
  operation** (since 4.0.0 `SET NX EX` took two locks), **writes made
  after a crash inside a transaction survive the next restart** (since
  4.0.0), and **a hash field's own TTL survives a background AOF rewrite**
  (since 3.0.0).
- **`COPY` copies a key of any type** (since 2.0.13 only strings).
- **Opening is about 2.8× faster and closing returns at once.**

Downgrading: 6.4 does not read the staging file, and loses what a killed
7.0 process left in it. After an app has been killed on 7.0, open and close
the store cleanly with 7.0 before installing a 6.4 build. 6.4 also opens a
directory 7.0 wrote without its indexes, views and tables. See
[the upgrade guide](https://github.com/goliajp/kevy/blob/develop/docs/upgrading-6.4-to-7.0.md).
