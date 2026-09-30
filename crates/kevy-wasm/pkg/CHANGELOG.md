# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Tracks the kevy 7.0.0 engine. No API change in this package; `kevy.wasm`
is built from 7.0.0. What changes underneath:

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
IndexedDB, as before. Streams and geo, which the native embedded engine now
serves, are not in this build.
