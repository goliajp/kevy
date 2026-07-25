## 4.0.0

* First tracked release of the kevy Flutter door, aligned with the kevy 4.x
  engine.
* `KevyDb` over `dart:ffi`: scalar `get`/`set`/`getText`/`setText` with TTL,
  `incrBy`, `expire`, `pttlMs`, `keys`, `flushAll`, and `cmd()` to every verb.
* Persistence: `KevyDb.open(dir)` (AOF + snapshot) and in-memory stores.
* Pub/sub: `publish`, `subscribe`/`psubscribe` → `KevySub`.
* `KevySub.waitNext(timeoutMs)` — blocking receive that parks in the kernel
  (`kevy_sub_wait`) instead of spinning the polled `next()`.
