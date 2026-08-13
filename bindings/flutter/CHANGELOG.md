## 5.1.0

Tracks the kevy 5.1.0 engine. No API change in this door — the vendored
engine is what moved.

* **Fixes a corruption path.** In 5.0.0 a compressed value could read
  back as a decode error after the value log compacted it: the encoder
  tagged a literal-only frame as dictionary-dependent when the
  dictionary carried a shared Huffman table, and its own decoder then
  refused it. The CRC covers the bytes that were written, so nothing
  catches it at write time. If you enable compression or value logging,
  this is the reason to move.
* Tail latency: the reactor no longer stalls on the rewrite hand-off,
  and the durability queue no longer drains on every tick.
* Replication generations are random identities rather than counters,
  so two nodes can no longer collide on one.

## 5.0.0

Tracks the kevy 5.0.0 engine — the tail-latency release. Element-level
copy-on-write for collections, off-thread rewrite completion, and
group-committed durability. Data directories from 4.x open unchanged.

## 4.0.0

* First tracked release of the kevy Flutter door, aligned with the kevy 4.x
  engine.
* `KevyDb` over `dart:ffi`: scalar `get`/`set`/`getText`/`setText` with TTL,
  `incrBy`, `expire`, `pttlMs`, `keys`, `flushAll`, and `cmd()` to every verb.
* Persistence: `KevyDb.open(dir)` (AOF + snapshot) and in-memory stores.
* Pub/sub: `publish`, `subscribe`/`psubscribe` → `KevySub`.
* `KevySub.waitNext(timeoutMs)` — blocking receive that parks in the kernel
  (`kevy_sub_wait`) instead of spinning the polled `next()`.
