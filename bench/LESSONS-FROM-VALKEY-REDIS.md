# Lessons from valkey 9.1 + redis 8.8 source (2026-06-21)

After running the 4-way matrix bench
(`bench/MATRIX-2026-06-21.md`) — kevy leads c1-P1 huge (98 k SET vs
valkey 67 k vs redis 43 k) but **valkey edges kevy by 3-11 %** in several
concurrent scenarios (c50-P1 GET, c50-INCR, c50-MGET, c50-P16 GET).
Read both source trees to learn what's behind each delta and what
ports to kevy.

## Big architectural finding (redis 8.8 vs valkey 9.1)

**redis 8.8 `iothread.c`** uses per-thread `list` queues with one
`pthread_mutex_t` per thread. Main thread coordinates. Lots of
lock acquisitions on the steady state.

**valkey 9.1 `io_threads.c`** uses:
- **lock-free SPMC shared inbox** (Single Producer Multi Consumer) —
  main thread → any io thread can dequeue
- **lock-free MPSC outbox** — io threads → main thread
- **per-thread SPSC private inbox** for thread-pinned tasks
- **tagged pointers**: low 3 bits of every job pointer store the job
  type (requires 8-byte aligned data pointers, which jemalloc gives
  for free)
- atomic counters for job-flight tracking

This is why valkey beats redis 8.8 by **5-15 % across every scenario**
— same data structures + commands, much lower IPC overhead.

**kevy is architecturally aligned with valkey, not redis**:
shared-nothing thread-per-core + the in-house `kevy-ring` SPSC ring +
`Inbound` discriminated union for typed messages. The matrix
confirms kevy and valkey are within noise of each other on
concurrent workloads — the architecture choice was correct.

## Lessons worth porting to kevy

### L1 — `tryAvoidBulkStrCopyToReply` (zero-copy bulk reply)

`networking.c:1463-1495`. When the value is RAW-encoded, valkey
stores a *pointer to the original robj* in the reply linked list
instead of memcpy-ing the bytes. `writevToClient` later builds an
iovec list of `[$len\r\n, value_ptr, \r\n]` per bulk and submits
**one** `writev()`. For a 1 KB GET this avoids the 1 KB memcpy.

**Match in kevy code**: `kevy_resp::encode_bulk` writes header +
data + CRLF into `conn.output: Vec<u8>` — one memcpy of the entire
value per GET. The 10 KB GET scenario in the matrix is exactly where
this matters; kevy GET 156 494 ties valkey 155 884 only because
other parts of kevy compensate.

**Port plan (substantial):**
1. Add `OutputFrag` enum: `Borrowed { ptr, len }` (points into the
   keyspace value's SmallBytes), `OwnedBytes(Vec<u8>)` (header
   scratch + literal CRLF), `Pooled` (recycled inline arena).
2. `Conn.output` becomes `Vec<OutputFrag>` instead of `Vec<u8>`.
3. The write path in `uring_arm_conns` builds an `&[iovec]` and uses
   `IORING_OP_WRITEV` instead of `IORING_OP_WRITE`.
4. Lifetime guard: a borrowed frag pins its source SmallBytes via
   Arc clone (kevy-store values are Arc<Bytes>-equivalent already);
   the Arc holds the bytes valid until the frag's writev CQE fires.

**Risk**: significant restructure of the write path. Worth doing
only if a large-value or pipelined-GET workload is the target.

### L2 — `OBJ_ENCODING_INT` for INCR fast path

`object.c:createStringObjectFromLongLongForValue` + the INCR fast
path in `t_string.c:incrDecrCommand` (line 713-715):
```c
if (o && o->refcount == 1 && o->encoding == OBJ_ENCODING_INT &&
    value >= LONG_MIN && value <= LONG_MAX) {
    new = o;
    objectSetVal(o, (void *)((long)value));
}
```
When a string value is a `long` that fits, valkey stores the integer
**directly in the value pointer slot** (cast `long → void*`). INCR
becomes `*ptr += delta` — no parse, no format, no allocation.

**Match in kevy code**:
`kevy_store::string::Store::incr_by` always does
`parse_i64(v.as_slice()) → ... → next.to_string().into_bytes()
→ SmallBytes::from_vec(...)`. Per-INCR cost: ASCII parse + ASCII
format + SmallBytes allocation.

**Port plan (medium):**
1. Add `Value::Int(i64)` variant alongside `Value::Str(SmallBytes)`.
2. `SET` checks if the value parses as a tight i64 and stores `Int`
   instead of `Str` (Redis behavior).
3. `GET` formats `Int` → ASCII on the way out.
4. `INCR` on `Int`: in-place `+= delta`; on `Str`: existing path
   (parse → mutate → could promote to `Int` for next time).
5. Storage size: `Value::Int(i64)` is 8 bytes; `Value::Str(SmallBytes)`
   is 24 bytes inline. The enum tag costs the union 24+1+padding = 32 B
   per value (same as today's enum if Str is the largest). No regression.

**Expected gain**: 3-8 % on INCR-heavy workloads (matrix c50-INCR is
3 % gap; INCR-only workloads more).

### L3 — `writevToClient` gather over reply list (`networking.c:2707`)

valkey accumulates replies into a **linked list of reply blocks** +
a small static buffer, then on flush builds an iovec array spanning
ALL of them and calls one `writev()`. The reply list lets large
replies span N blocks without resizing one big buffer; writev fuses
them into one syscall.

**Match in kevy code**: `Conn.output: Vec<u8>` is one growing
buffer; `prep_write` submits one contiguous range. A spike that
needs to grow the Vec costs O(n) memcpy.

**Port plan (medium, related to L1):**
Switch to a deque of reply blocks (or use `OutputFrag` enum from
L1). Each command's reply lands in a fresh block; writev gathers
on flush. Avoids the Vec resize/memcpy on bursts.

**Expected gain**: visible on pipelined workloads (c50-P16 GET is
where valkey beats kevy by 11 % despite kevy leading SET — the
reply path is the gap).

### L4 — listpack for small collections (`listpack.c`, valkey/redis)

`listpack` is a packed byte-array encoding for small lists / hashes
/ sets / sorted-sets. Up to a config threshold (e.g. 128 entries,
each ≤ 64 B), the entire collection is stored as a single
contiguous byte slice — no separate node allocations, no pointer
chains. Reading is sequential scan; mutations rewrite the slice.
Beats a HashMap / linked list for L1 locality and per-element alloc
amortization on small collections.

**Match in kevy code**: kevy's collection types use full
`HashMap` / `Vec` from the start regardless of size.

**Port plan (large)**: implement listpack as a new kevy crate
(0-dep), thread through hash/list/set/zset value types with
encoding-upgrade-on-threshold logic. Multi-week scope.

**Out of current scope** — only worth doing when collection
workloads (HMGET, ZRANGE, etc.) are the target. Note for future.

### L5 — `connWritev` retry loop + partial-write resumption

`networking.c:writevToClient` handles partial writes by tracking
`c->io_last_written_data_len` and rebuilding the iovec from where
the previous writev stopped. Avoids re-encoding on partial flushes.

**Match in kevy code**: `uring_on_write` advances `write_off` and
resubmits the rest next iter. Same idea, simpler shape (no list of
blocks to skip). Functionally equivalent. **No port needed.**

### L6 — Hot-loop config knob: `IO_THREAD_MAX_PENDING_CLIENTS`

valkey batches "send pending client back to main" until the queue
hits a threshold OR the main thread is idle. Reduces wakeup IPI
storms.

**Match in kevy code**: kevy's `flush_wakes` already does this via
`pending_wakes` bitmap + the SeqCst fence pairing. Functionally
equivalent. **No port needed.**

## What NOT to port

- redis 8.8's per-thread mutex io_threads — strictly worse than
  valkey's lock-free queues, and kevy's `kevy-ring` SPSC is even
  better matched to thread-per-core.
- valkey's `client *c` mega-struct (linked-list-everything) — that's
  C-language workaround for not having Rust enums. kevy's
  `Inbound` discriminated union is the equivalent done right.
- Lua scripting (eval) — separate workstream; see [[project-lua-runtime-direction]].

## Priority

| Lesson | Effort | Expected gain | Where it'd show up        |
|--------|--------|---------------|---------------------------|
| L2 INT encoding | medium | 3-8 % | INCR-heavy workloads          |
| L1 zero-copy bulk | large | 5-15 % | GET large values + pipelined |
| L3 writev gather | medium | tied with L1 | pipelined replies          |
| L4 listpack | large | n/a in default bench | small-collection workloads |
| L5 partial-write | done | — | already equivalent          |
| L6 wake batching | done | — | already equivalent          |

**Recommended sequence:**
1. L2 INT encoding first — focused, well-bounded, real INCR win.
2. L1 + L3 together (they share the OutputFrag/writev restructure)
   — biggest userspace lever remaining, but invasive write-path
   surgery. Plan as a v1.25 feature.
3. L4 listpack — workload-driven, defer until small-collection
   benchmark is the target.

## Reproduce the source study

```bash
ssh lx64
cd /root/srcbench/valkey
wc -l src/networking.c src/t_string.c src/io_threads.c
# Read addReplyBulk (line ~1470), incrDecrCommand (~697),
# writevToClient (~2707), tryAvoidBulkStrCopyToReply (~1462).

cd /root/srcbench/redis
wc -l src/networking.c src/t_string.c src/iothread.c
# Compare iothread.c (lock-heavy) vs valkey io_threads.c (lock-free).
```
