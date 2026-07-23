# RFC — the embedded bulk-write ceiling: per-op durable log vs deferred-commit

**Date:** 2026-07-24 · **Roadmap:** v4 t4 (embedded bench) Attack #1 follow-up ·
**Status:** DECISION — Phase A decomposition complete; the recommendation is
**keep the per-op durable-log model; do not add a deferred-commit batch mode.**
This document records why, so it is not re-litigated.

Companion to `bench/EMBEDDED-LEDGER.md` (the measured head-to-head) and the
`kevy_set_many` batch primitive already landed (`crates/kevy-ffi/src/batch.rs`).

## The gap, measured

Against five embedded engines (SQLite, bbolt, badger, LMDB ×2), kevy wins
single-op reads (its zero-copy `kevy_get_shared` lane beats even LMDB, the
read-latency leader) and single-op small writes. It **loses batched/bulk
writes**, and the loss is engine-level, not binding-level — confirmed on two
bindings that pay no expensive crossing:

| batch SET 16 B, T-async | kevy | peer | gap |
|-------------------------|-----:|-----:|----:|
| C (no FFI crossing)     | ~219 ns | LMDB ~80 ns | 2.7× |
| C# (LibraryImport, cheap crossing) | ~200 ns | LMDB ~80 ns | 2.5× |

`kevy_set_many` does **not** move these (there is no crossing to amortize —
it only helped kevy-go, whose cgo boundary is ~50–100 ns/op, taking its
16 B batch SET from 147× off bbolt to 2.7×, i.e. down to this same engine
floor). So the floor is the engine.

## Decomposition — where kevy's ~200 ns/op goes vs LMDB's ~80 ns

kevy per-op SET (16 B, EverySec, in-process):
1. **Store insert** — `KevyMap` hashmap insert + `Value::Str` (inline ≤ 22 B,
   no alloc). ~40–60 ns.
2. **AOF frame format** — build the RESP frame `*3\r\n$3\r\nSET\r\n$k\r\n<key>\r\n$v\r\n<val>\r\n`:
   itoa the three length prefixes, write the structure. ~60–100 ns.
3. **BufWriter append** — memcpy the frame into the 256 KiB AOF buffer. No
   per-op fsync (EverySec flushes once a second). ~20 ns.

LMDB per-op in a bulk txn (`MDB_NOSYNC`):
1. **B+tree descent + write into a dirty page** — ~40–60 ns.
2. **Nothing else per op.** No log frame. Durability is the *deferred*
   single commit at the end of the txn (and with `NOSYNC`, not even fsynced —
   the OS flushes the dirty mmap pages whenever).

**The gap is step 2: kevy formats and appends a durable log frame per write;
LMDB writes a dirty page and defers all durability to one commit.** That per-op
frame is not an accident — it is kevy's durability model: **every write is in
the crash-recoverable AOF the moment it returns** (bounded loss ≤ 1 s at
EverySec, 0 at Always). LMDB's bulk txn has *no* durability until commit — a
crash mid-batch loses the whole batch.

## Why batching cannot close it without changing the model

- **The frame format is inherently per-op.** Each SET has its own key/val
  lengths; a batch still formats N distinct frames. No amortization.
- **The store insert is inherently per-op** (one hashmap insert per key).
- **The fsync is already batched** at EverySec (once/second, not per op) — so
  there is no per-op fsync for a batch to coalesce. (At Always durability a
  batch *could* coalesce the fsync — that is the one real batch win, and
  `kevy_set_many` already does it via the AOF group-commit path; but the
  T-async headline tier has no per-op fsync to save.)

So the only way to reach LMDB's ~80 ns is to stop writing a durable frame per
op — i.e. adopt a **deferred-commit** model: buffer N writes with no per-op log
entry, and write one combined durability record at commit. That is a different
product.

## The options

1. **Deferred-commit batch mode (`Store::txn` / a "bulk load" mode).** N writes
   accumulate in a staging buffer with no AOF frame per op; on commit, one
   combined AOF record (or a snapshot-style bulk append) is written and fsynced.
   Closes the gap to LMDB. **Cost:** a crash before commit loses the whole
   batch — kevy's per-op-recoverability guarantee is gone for writes in the
   batch. Also: readers in the same process see the writes immediately (the
   store hashmap is live), but the AOF doesn't have them until commit, so a
   crash-replay diverges from what readers saw. That reader/durability skew is
   the hard part, and it is exactly the invariant the per-op log avoids.
2. **Faster per-op frame (micro).** Shave step 2 with a tighter itoa / a
   pre-sized frame writer. Realistic gain ~10–20 ns/op — does not close 200→80,
   and the mmkvgate SET decomposition already found the itoa header is not the
   bottleneck (the memcpy + store insert scale with data, the header does not).
   Not worth a campaign.
3. **Keep the per-op durable-log model. Accept the bulk-write gap as the
   honest price of per-op durability.** Document it; point bulk-load users at
   `kevy_set_many` (crossing amortization + Always-mode fsync coalescing) and
   at the RDS on-ramp import path (`kevy-cli import`, which is already the
   blessed bulk path and can stage differently).

## Recommendation — Option 3

**Keep per-op durable logging; do not add a deferred-commit batch mode.**

- kevy's identity (serving engine, "every write immediately recoverable",
  the durability-trust arc t5.5) is the per-op durable log. A deferred-commit
  mode that loses a batch on crash contradicts the guarantee the durability
  arc just hardened and that the goliajp embedded-as-primary-store consumer is
  relying on. Trading it for LMDB-parity on bulk *write throughput* is the
  wrong trade for a store people put payroll/financial data in.
- The bulk-write gap is **2.5–2.7× at small values** (not orders of
  magnitude) and only widens for large values via the copy — a bulk load of
  many small keys (the RDS on-ramp shape) is 2.5×, acceptable against an
  engine that offers *no* durability until commit.
- The read side — where kevy already **beats** LMDB — is the ceiling that
  matters for a serving engine (read-dominated). That win is real and kept.

If a future consumer genuinely needs LMDB-class bulk-load throughput with
explicit "durability at commit" semantics, that is a **new, opt-in `Store`
transaction API** with its own RFC and its own crash-recovery test matrix —
not a silent change to how `set` durably logs. Named here as a possible future
train, deliberately not built now.

## What this closes

- Attack #1 (batch-write) is **complete**: the binding-crossing half is closed
  (`kevy_set_many`, kevy-go/C# `SetMany`); the engine half is decomposed to its
  root (per-op durable frame) and **decided** (keep it). No open "add a batch
  mode" action — it is a considered no, not a to-do.
- The remaining embedded-bench ceiling item is **Attack #2** (zero-copy binding
  read lanes: the engine read already beats LMDB; the copying Go/C# bindings
  give it back at large values). That is a binding-API + lifetime-safety design,
  tracked separately.
