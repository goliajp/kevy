# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Tracks the kevy 7.0.0 engine; `kevy.wasm` is built from 7.0.0. The loader API
is unchanged. What changes:

- **Streams and geo.** `cmd` now reaches the stream commands (`XADD`,
  `XRANGE`, `XREAD`, `XGROUP`, `XREADGROUP`, `XACK`, `XPENDING`, `XCLAIM`,
  `XAUTOCLAIM`, `XINFO`, …) and the geo commands (`GEOADD`, `GEOSEARCH`,
  `GEODIST`, …). A read with `BLOCK` answers `ERR the embedded engine cannot
  block; call without BLOCK`: a tab has one thread, and parking it would
  freeze the page. The module is 602 KB gzipped, up from 539 KB without them.
- **Writes through `cmd` persist.** Since 4.0.0 they never reached the log,
  and were lost at the next `open()` unless a compaction had run. They now
  reach it as the frames a native AOF would hold (an `XADD *` with the id it
  chose, a group read with the deliveries it made), so a stream and its
  consumer groups survive a reload.
- **Declared indexes, views and tables survive a reload.** Since 5.2.0,
  when the browser build gained them, they were lost at every `open()`:
  the rows came back without the indexes over them. The log and the compacted image now carry the catalog, and the
  reload rebuilds the indexes from the replayed keys.

- **A hash field's own TTL survives log compaction.** Since 3.0.0 the log
  image the loader writes when it compacts carried every value and key TTL
  but not the per-field deadlines set with `HEXPIRE` and its siblings, so
  after a compaction and a reload those fields never expired.
- **`COPY` copies a key of any type.** Since 2.0.13 only strings were
  copied; other types answered `WRONGTYPE`.
- **`INCRBYFLOAT` answers with the stored value's own digits,** as the
  server does.

The mapped log and the staging file that the native embeddings gained in
7.0 do not apply here: the log is written by the loader, through OPFS or
IndexedDB, as before.
