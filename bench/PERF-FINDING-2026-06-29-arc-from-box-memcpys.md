# 2026-06-29 — `Arc::from(Box<[u8]>)` memcpys; B3 cannot close the bigval-SET gap

Negative finding from implementing the v1.29 RFC's B3 attack (C1+C2+C3 chain).
Phase A decomp's premise was wrong; the fix doesn't sit where the decomp said.

Anchor: [`.claude/rfcs/2026-06-29-v1-29-bigval-owned-bytes.md`](../.claude/rfcs/2026-06-29-v1-29-bigval-owned-bytes.md) (the now-superseded RFC) + the C1 enabler commit `33b9d9b`.

## What the Phase A decomp predicted

Phase A decomp ([`PERF-DECOMP-2026-06-28-bigval-SET-vs-valkey-9.1.md`](PERF-DECOMP-2026-06-28-bigval-SET-vs-valkey-9.1.md)) said kevy's `-d 65536` SET does TWO userspace memcpys of the 64 KiB body that valkey avoids:

- **MEMCPY #1**: slab → frame Vec at `uring_bigbulk.rs:87/115` (`extend_from_slice`)
- **MEMCPY #2**: frame → Arc at `string.rs:38` (`Arc::from(&[u8])` inside `pick_value_for_set`)

The RFC's B3 attack proposed eliminating MEMCPY #2 by routing an owned `Vec<u8>` into `store.set(key, body, …)` → `pick_value_for_set_owned(body)` → `Arc::from(body.into_boxed_slice())`. The comment on `pick_value_for_set_owned` claimed this is zero-copy because "`Arc::from(Box<[u8]>)` wraps without copying".

## What I actually measured

Implemented C2 (split `BigArgState` into `Frame` + `BareSet` variants) and C3 (local-shard bare-SET fast path) on `feature/v1-29-bigval-owned-bytes`. Built `release-perf` binary on lx64 + ran axis B at `-c 50 -P 1 -d 65536 -t set -n 800k`, with `perf record -F 999 --call-graph fp -p <kevy_pid> sleep 10`.

### Throughput

| Build | -d 65536 SET (median of 3) |
|---|---|
| v1.28 baseline (kevy v1.24 source) | 64,143 SET/s |
| v1.29 C2+C3 (this branch) | 64,745 SET/s (+0.9%) |
| valkey 9.1 | 67,196 SET/s |

### CPU profile

| Symbol | v1.28 baseline | v1.29 C2+C3 |
|---|---|---|
| libc `__memcpy_avx_unaligned_erms` | 15.92% | **22.85%** (**↑ 6.93 pp REGRESSED**) |
| kernel `rep_movs_alternative` | 16.31% | 16.05% |
| `Runtime::run` / `run_uring` | (was unresolved 0x6c56c at 9.54%) | 5.48% (now resolved by debug-symbol rebuild) |

**The userspace memcpy fraction went UP, not down**. Treatment regressed against control by 6.93 percentage points.

## Why

Two reasons identified by reading the implementation against the Rust std source:

### Reason 1 — `Arc::from(Box<[u8]>)` is not zero-copy

`pick_value_for_set_owned`'s comment is wrong. The actual Rust std impl:

```rust
impl<T> From<Box<[T]>> for Arc<[T]> {
    fn from(v: Box<[T]>) -> Arc<[T]> {
        unsafe { Self::copy_from_slice(&v) }
    }
}
```

`Arc<[T]>` is backed by `Arc<ArcInner<[T]>>` where `ArcInner<T> = { strong: AtomicUsize, weak: AtomicUsize, data: T }`. The data field's offset within the allocation is non-zero (it's after the two refcount words). A `Box<[u8]>`'s data starts at offset 0. So the two memory layouts are incompatible — `Arc::from(Box)` MUST allocate a fresh buffer with the correct refcount header and `copy_from_slice` the bytes.

Net effect: the path `Vec<u8> → into_boxed_slice() → Arc::from(Box<[u8]>)` does the SAME 64 KiB memcpy that `Arc::from(&[u8])` did. The B3 attack doesn't reduce memcpys on the local-shard fast path; it shuffles them between callsites.

### Reason 2 — cross-shard fallback adds a memcpy

C2+C3's `uring_apply_bigarg::BareSet` arm falls back to `synthesize_set_frame(&key, &body)` when `shard_of(&key) != self.id`, which builds a new RESP frame Vec containing the body bytes (one additional memcpy of 64 KiB) for dispatch_batch to re-parse. At `--threads 2` (the bench config), ~50% of SETs hash off-shard, so the cross-shard path runs half the time. This is where the 6.93 pp memcpy regression comes from.

The cross-shard regression is fixable (promote to `Frame` variant from the start when `shard_of(&key) != self.id`), but even after that fix, Reason 1 leaves the local-shard path with the same memcpy count as v1.28 → ZERO net gain.

## What actually closes the gap

`Arc<[u8]>` is the wrong value-type for zero-copy bigval-SET. Two paths forward:

### Option A — Value type change (invasive)

Change `Value::ArcBulk` from `Arc<[u8]>` to `Arc<Box<[u8]>>` (or `Arc<Vec<u8>>`).

- `Arc::new(box)` is genuinely zero-copy: `Arc<T>` allocates an `ArcInner<T>` containing `{ strong, weak, data: T }`. When `T = Box<[u8]>`, the Box wrapper inside the Arc's data slot points at the original heap buffer; the buffer itself stays put.
- Per-GET cost: one extra pointer dereference (`Arc<Box<[u8]>>::as_ref()` → `&Box<[u8]>::as_ref()` → `&[u8]`). Tiny.
- Per-Arc allocation overhead: 24 bytes (2 refcounts + Box ptr) vs current 16 bytes (2 refcounts + DST length). Negligible.
- Touches `kevy_store::Value` (the keyspace value enum), every consumer of `Value::ArcBulk`, the writev iovec path that points at the inner bytes, GET-reply emission, snapshot/AOF persistence code, and any `mem::size_of::<Value>()`-sensitive layout assumption. Multi-crate refactor across kevy-store / kevy-bytes / kevy-rt / kevy-persist / kevy-rdb.

### Option B — B2-alt (kernel-side direct copy into the recv buffer)

The original B2-alt from the RFC (declared out-of-scope for v1.29.0):

- On `try_promote_bigbulk` Promote: cancel the multishot recv on this conn, allocate `Vec::with_capacity(bulklen)`, submit a single-shot `prep_read(fd, buf_ptr, bulklen)` SQE. The kernel writes recv bytes DIRECTLY into the destination Vec — no slab involvement, no slab→body memcpy.
- 150 LOC across `kevy-rt` + `kevy-uring`. Risks: multishot-cancel-and-rearm semantics under in-flight CQEs.
- Eliminates MEMCPY #1 entirely. MEMCPY #2 still exists unless paired with Option A (or accepts the unavoidable Arc-layout copy).

### What does valkey actually do?

valkey's `sds` is a heap-allocated buffer whose header sits BEFORE the data pointer (the sds pointer points at the data, with the length / cap header in negative offsets). The recv path calls `read(fd, sdsbuf, len)` directly into the sds data area — kernel writes user-visible data straight to the final value buffer. **Zero userspace memcpy of the body bytes**. This is structurally equivalent to Option B (`prep_read` direct-into-buffer) in kevy.

## Decision

Revert C2+C3. Keep C1 (probe surface extension is a harmless enabler for future attacks). v1.29.0 ship is OFF the B3 table.

The genuine next attack is Option B (B2-alt) — kernel-side direct-into-buffer recv via single-shot `prep_read`. Option A is the only way to ALSO eliminate MEMCPY #2; it's worth doing if Option B alone doesn't close the gap (per the perf record, kernel memcpy at 16% is the bigger bucket than userspace at 16% so eliminating both yields a non-trivial perf win).

The `pick_value_for_set_owned` comment claiming Arc-from-Box is zero-copy should be corrected in tree.

## Methodology lesson

Per `feedback-perf-vs-foss-decomposition.md` §1 + the global perf methodology doc §6:

- **"Decomposition is DISCOVERY not CONFIRMATION."** Phase A's source-only finding of "Arc::from(box) is zero-copy" was wrong; runtime verification via implementation+perf was the only way to learn it. Source-only is necessary NOT sufficient — a lesson the methodology doc records (luna fib_28 case) and this is now a second case study.
- **"REVERT is honest, not failure."** C2+C3 reverted; the wasted implementation work surfaces a real structural constraint that next session uses to pick the right attack.
- **"Don't ship a regression."** Even +1% gain wouldn't justify shipping a path that increases userspace memcpy fraction by 6.93 pp on the cross-shard fallback.

## State left in tree

- C1 commit `33b9d9b` stays on `feature/v1-29-bigval-owned-bytes`. The probe surface extension (`body_start_in_tail` / `body_len` / `bare_set_key_range` on `BigArgGenericProbe::Promote`) is harmless `#[allow(dead_code)]` and enables either Option A or Option B implementations without re-doing the probe walk. Tests stay green.
- C2+C3 working-tree changes reverted; not committed.
- RFC `.claude/rfcs/2026-06-29-v1-29-bigval-owned-bytes.md` superseded — see this doc for the actual finding + next steps.
