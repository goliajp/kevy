## 6.1.0

Tracks the kevy 6.1.0 engine. No API change in this door.

* **The engine this door ships no longer names who built it.** Every
  prebuilt `libkevy_ffi.so` carried the build machine's home directory —
  27 paths per Android library — because a toolchain with the `rust-src`
  component resolves std's panic locations to the local source tree.
  `packaging/android/build-*jnilibs.sh` remaps it away now, and the
  alignment gate refuses an artifact that names its builder.

## 6.0.0

Tracks the kevy 6.0.0 engine. No API change in this door.

* **Fourteen commands reach the server wire.** `SETBIT`, `GETBIT`,
  `BITCOUNT`, `BITPOS`, `BITOP`, `GETRANGE`, `SETRANGE`, `LINSERT`,
  `COPY`, `TOUCH`, `TIME`, `GETEX`, `ZREVRANGE`, `HINCRBYFLOAT`. The
  engine had always implemented them and this door's embedded half had
  always answered them; a RESP client got `unknown command`. That is
  closed, so the two halves of this door now answer the same set.

* **A range past the end is empty.** `GETRANGE k 99 200` on a shorter
  value, and `ZREVRANGE z 5 10` on a smaller set, answered the last
  element where Redis answers nothing. Both halves of this door shared
  the mistake, which is why comparing them did not find it — a
  three-way differential against a real valkey did.

* **`MGET` answers nil for a wrong-typed key** rather than failing the
  whole call, which is what its own documentation always said.

The vendored engine is rebuilt and self-reports 6.0.0.

## 5.4.1, 5.4.0

No entries were written for these. The door shipped with the version
bumped and its changelog left at 5.3.0, which is what
`tools/check_door_changelogs.py` now refuses: `dart pub publish` warns
about it, and a warning nobody reads is how two releases went by.

## 5.3.0

Tracks the kevy 5.3.0 engine. No API change in this door — 5.3's core
deliverable is the engine's own test system (a three-tier audited
suite), and its one product fix is in `kevy-cli`, which this door does
not ship. The vendored engine is rebuilt and self-reports 5.3.0.

## 5.2.0

Tracks the kevy 5.2.0 engine. No API change in this door.

* **Lua dialect corrections.** The embedded scripting engine moved to
  luna-core 3.0.0: each Lua dialect now serves its own table surface
  and error wording, and the `string.rep` denial-of-service fix from
  the 5.1 line is pinned by a dedicated gate.

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
