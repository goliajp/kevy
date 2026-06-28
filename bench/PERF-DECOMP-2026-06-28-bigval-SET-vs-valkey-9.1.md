# `-d 65536` SET decomposition — kevy v1.28 vs valkey 9.1.0

Date: 2026-06-28
Workload: `redis-benchmark -c 50 -P 1 -n 200000 -t set -d 65536` against
both servers on TCP loopback, lx64 (16-core x86_64, Linux 6.12,
mitigations=off). kevy: `--threads 2` taskset `-c 0-1`; valkey 9.1.0:
`--io-threads 10 --io-threads-do-reads yes` taskset `-c 0-9` (i.e.
thin-kevy vs fat-valkey, per the axis-sweep probe setup).

Phase A read-only research per
`~/.claude-shared/global/methodology/perf-decomposition-vs-polish.md`.
**No code edits — attack candidates are concrete file:line references to
be implemented in Phase B**.

This decomp targets the single biggest workload-shape gap surfaced in
[`PERF-PROBE-2026-06-28-axis-sweep-vs-valkey.md`](PERF-PROBE-2026-06-28-axis-sweep-vs-valkey.md):
**axis B `-d 65536` SET: kevy 63.6k vs valkey 69.3k → -8% LOSS**.

---

## Measured baseline

From the axis-sweep probe (medianed N=3, RUNS=3):

| op | kevy(2c) | valkey(10c) | gap (rps) | per-op time |
|---|---|---|---|---|
| `-d 65536` SET | 63,613 | 69,252 | −5,639 (−8.1%) | kevy 15.72 µs/op, valkey 14.44 µs/op |
| `-d 65536` GET | 69,832 | 71,839 | −2,007 (−2.8%) | kevy 14.32 µs/op, valkey 13.92 µs/op |

Per-op delta: **1.28 µs/op slower on kevy for SET, 0.40 µs/op slower
for GET**. The asymmetry (SET loss = 3× the GET loss) is the principal
clue: GET sends a 64 KiB reply but receives a tiny request; SET sends a
5-byte `+OK\r\n` reply but **receives a 64 KiB body**. The 0.9 µs
delta-of-deltas (1.28 − 0.40) must be attributable to **inbound-side
work specific to SET's 64 KiB body**.

These are wire-time numbers at `-c 50 -P 1`: the loopback fabric carries
~50 in-flight requests at any moment, so per-op server CPU is amortized
across the 50-way pipeline. The 1.28 µs gap is therefore a delta of
**incremental** server work per op, not raw single-op CPU.

---

## Atomic op cost table (lx64 x86_64, Linux 6.12, mitigations=off)

Reused from the c100-GET decomp (`PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md`),
plus memcpy throughput row for the 64 KiB regime:

| Op | Cost |
|---|---|
| L1 cache hit | 1 ns |
| L2 hit | 4 ns |
| L3 / DRAM | 60-120 ns |
| Atomic load (Acquire, uncontended) | 2 ns |
| Heap alloc (jemalloc small) | 35-60 ns |
| Heap alloc + zero (jemalloc, 64 KiB) | 150-300 ns |
| `memcpy` 64 KiB, L2-hot (one side hot) | 3,500-7,500 ns (≈ 10-18 GB/s effective) |
| `memcpy` 64 KiB, L3 / cold | 6,500-13,000 ns (≈ 5-10 GB/s effective) |
| `read`/`write` 16 KiB on TCP loopback | 1,200-3,500 ns |
| `io_uring_enter` (COOP_TASKRUN, no submit) | 250-500 ns |

The single memcpy cost row dominates everything else in a `-d 65536`
decomp — a single 64 KiB userspace memcpy already costs more than the
entire 1.28 µs/op gap.

---

## §A — Recv-vs-Send asymmetry test (the framing question)

Probe hypothesised the loss is "single large bytes leaving the server"
(GET reply / pubsub broadcast). For SET the **reply is 5 bytes**, so
that framing cannot literally explain the SET-specific 8% loss. Test:
is SET's loss on the **inbound side** instead?

**(a) Does kevy do an inbound userspace memcpy that valkey avoids?**

Valkey (`networking.c:4198 readToQueryBuf` + `:3742-3805 parseMultibulkBuffer`):

- At the first `read()` for this SET, `c->querybuf` is `thread_shared_qb`
  (empty). After the first chunk lands, the parser sees `bulklen=65536 ≥
  PROTO_MBULK_BIG_ARG=32 KiB` (server.h:214) → enters the big-arg path
  (`networking.c:3742-3769`):
  1. `sdsrange(c->querybuf, c->qb_pos, -1)` shifts the header off, so
     `qb_pos=0` and the querybuf now holds only the partial body.
  2. `sdsMakeRoomForNonGreedy(c->querybuf, ll+2 - sdslen(c->querybuf))`
     **sizes the querybuf to exactly bulklen+2 = 65 538 bytes**. One
     non-greedy realloc; subsequent `read()`s fill into this sds with
     **zero userspace memcpy of value bytes** — the kernel writes
     straight from socket buffer into the sds heap.
- When the body completes (`networking.c:3797-3810`), the parser hits
  the fast adoption arm:
  ```c
  if (!is_replicated && c->qb_pos == 0 && c->bulklen >= PROTO_MBULK_BIG_ARG &&
      sdslen(c->querybuf) == (size_t)(c->bulklen + 2)) {
      (*argv)[(*argc)++] = createObject(OBJ_STRING, c->querybuf);
      *argv_len_sum += c->bulklen;
      sdsIncrLen(c->querybuf, -2); /* remove CRLF */
      c->querybuf = sdsnewlen(SDS_NOINIT, c->bulklen + 2);
      sdsclear(c->querybuf);
  }
  ```
  **`createObject(OBJ_STRING, c->querybuf)` ADOPTS the sds** as the
  value robj — zero copy of the 64 KiB body. A fresh `sdsnewlen(NULL,
  65 538)` is then allocated for the next request (sized identical
  because "we'll see another fat arg likely").
- `setCommand` → `setKey` → adopts the val robj into the db via
  `setGenericCommand` (t_string.c:146). No further copy of value bytes.

**Userspace memcpys of the 64 KiB value bytes on valkey: 0.**

kevy (`uring_io.rs:118-131` BigBulk routing + `uring_bigbulk.rs:66-125`
+ `string.rs:33-39 pick_value_for_set`):

- First slab CQE: `dispatch_batch(cid, slab)` parses the header, sees
  `*3 SET key $65536`, the body extends beyond the slab end → `consumed
  = 0`. `tail = &slab[..]`. `try_promote_bigbulk(cid, tail, io)` walks
  the header (`uring_bigbulk_probe.rs:179-240 probe_generic_bigbulk`),
  returns `Promote { total = 65 568, bytes_present = 16 404 }` (header
  ≈30 B + body up to slab boundary).
- `uring_bigbulk.rs:86-95`: `frame = Vec::with_capacity(65 568);
  frame.extend_from_slice(&tail[..16 404])` — **memcpy #1 of slab→frame
  on this CQE: 16 404 bytes** (~16 KiB).
- Subsequent recv CQEs go straight to `uring_on_recv` → BigBulk routing
  arm at `uring_io.rs:118-131` → `uring_bigbulk_feed` →
  `state.frame.extend_from_slice(&slab[..take])`. **Memcpy #1 spread
  across 4 CQEs: total 65 538 bytes of slab→frame.**
- On completion: `uring_apply_frame_stitch(cid, frame, io)` calls
  `dispatch_batch(cid, &frame)` (`uring_bigbulk.rs:168`). The frame
  is parsed AGAIN from scratch via the borrowed-argv parser, giving
  argv ranges into `frame`.
- `dispatch.rs:231 b"SET" => cmd_set(store, args, out)` →
  `cmd_data.rs:205 store.set_slice(&args[1], &args[2], …)` →
  `string.rs:88 pick_value_for_set(bytes)` → `string.rs:38
  Value::ArcBulk(Arc::from(bytes))`.
  - `Arc::from(&[u8])` allocates a new `Box<[u8]>` of size 64 KiB +
    16 B (Arc header) and **memcpys the bytes from frame into it**.
    **Memcpy #2: 65 536 bytes of frame→Arc.**

**Userspace memcpys of the 64 KiB value bytes on kevy: 2** (slab→frame
= 65 538 B, frame→Arc = 65 536 B). Net: **2 × 64 KiB ≈ 131 KiB of extra
userspace memcpy per SET**.

At a conservative ~25 GB/s L2-hot memcpy effective throughput, that
is ~5.2 µs of pure CPU spent on memcpys per SET. Wall-time at -c 50
amortizes this across the 50-way pipeline, so the visible gap is a
fraction of the raw memcpy cost — consistent with the measured 1.28 µs/op
delta.

**(b) Does kevy's parse re-allocate at 64 KiB inputs?**

No — kevy's `Vec::with_capacity(total)` at `uring_bigbulk.rs:86` is the
B.4 / A.2 v1.25 fix that retired the conn.input realloc storm. The
frame Vec is pre-sized to exact frame extent, so the four
`extend_from_slice` calls during BigBulk feed do not reallocate. The
realloc-storm hypothesis is **already addressed** in the shipped code.
However, the parser-induced second-walk over the assembled `frame`
(`uring_bigbulk.rs:168 dispatch_batch(cid, &frame)`) does perform
work — see S07b below.

**Verdict on framing**: the SET loss is **inbound-side**, not
reply-side. The 64-KiB writev (GET reply) and 64-KiB recv-then-Arc-copy
(SET inbound) are different code paths; SET's extra Arc-from-slice
memcpy is what makes SET worse than GET at the same byte size. The
probe's H1/H2/H3 (output-buffer growth / iovec assembly / TCP-MSS
multi-send) are all reply-side hypotheses and turn out to be **off the
target** for SET; the relevant hypothesis is H4 below.

---

## §B — Stage decomposition (per request, kevy vs valkey)

Stages are written **per SET request** for the bench shape `*3\r\n$3\r\nSET\r\n$<klen>\r\nkey:N\r\n$65536\r\n<65 536 bytes>\r\n` → server reply
`+OK\r\n` (5 bytes). Header total: ~30-35 bytes; body: 65 538 bytes
(including CRLF); reply: 5 bytes. Total wire per op: ~65 575 bytes
inbound + 5 outbound.

### S01 — kernel loopback TCP RX (client → server)

- **Reference (valkey)**: `tcp_v4_rcv` → `tcp_data_queue` → wakes the
  fd in `epoll_wait`. For a 64 KiB write at the client, the loopback
  delivers ~4 × 16 KiB skbs (loopback MTU is 65536 but at TCP_NODELAY
  the segmenting depends on cwnd; typical observed at -d 65536 is
  ~4-6 packets per request). Per-op kernel-half: ~1.5-2.5 µs.
- **Us (kevy)**: identical kernel path; multishot recv arms a permanent
  SQE so the kernel writes straight into the provided-buffer ring slabs
  (`uring_reactor.rs:52-53` pbuf = 4096 × 16 KiB). Per-op kernel-half:
  ~1.5-2.5 µs.
- **Δ**: 0 (shared kernel path).

### S02 — recv pickup (userspace notification)

- **Reference (valkey)**: `epoll_wait` returns the fd as readable
  (~800 ns / epoll_wait, amortized across c50 fds). Per-op: ~50-150 ns.
- **Us (kevy)**: io_uring CQE for the first arrival (multishot recv).
  CQE-read cost ~5-10 ns; the per-iter `io_uring_enter` is amortized
  via the `ENTER_SKIP_THRESHOLD=2` gate (`ring.rs:67`). Per-op: ~5-15 ns.
- **Δ**: kevy is **structurally ahead ~50-150 ns/op** here.

### S03 — recv: read syscall vs slab pickup (multi-chunk body)

The 64 KiB body crosses multiple read events (~4 chunks of ~16 KiB).

- **Reference (valkey)**: `connRead` → `read()` syscall per readable
  event. `networking.c:4243-4254` chooses non-greedy size = `bulklen+2 -
  qblen` for big_arg; subsequent reads fill into the same sds. **The
  read syscall writes kernel data straight into the sds heap** (the
  pre-sized buffer); no userspace memcpy of value bytes. Per chunk:
  ~1.5 µs syscall + ~0 userspace memcpy. Per op: ~6 µs across 4 reads.
- **Us (kevy)**: multishot recv CQE — the kernel writes into a slab
  in the provided-buffer ring (`ring.rs:439 for_each_completion`).
  Per chunk: ~5-10 ns CQE pickup + 0 userspace memcpy at this stage.
  Per op: ~30 ns across 4 CQEs.
- **Δ**: kevy is structurally ahead **~6 µs/op userspace** — but this
  is reclaimed (and then some) by S05 below. The crucial difference
  is **where the value bytes land**: valkey lands them directly into
  the sds that becomes the value robj; kevy lands them in a slab that
  must be copied OUT before the slab is recycled.

### S04 — RESP header parse on first chunk

- **Reference (valkey)**: `processInputBuffer` →
  `processMultibulkBuffer` (`networking.c:3557`) walks `*3 $3 SET $<k>
  key $65536`; finds bulklen=65536 ≥ PROTO_MBULK_BIG_ARG → trims
  querybuf to body-only, sizes querybuf via
  `sdsMakeRoomForNonGreedy` to bulklen+2. Per op: ~200-400 ns.
- **Us (kevy)**: `dispatch_batch(cid, slab)` runs the borrowed-argv
  parser (`request_borrowed.rs::parse_command_borrowed`). The parser
  sees `$65536` but only the first 16 KiB present → consumed=0 (no
  complete frame). Per op: ~150-250 ns (header walk + early bail on
  bulk-too-large). Then `try_promote_bigbulk` walks the same header a
  second time (`uring_bigbulk_probe.rs:179-240`): ~80-150 ns.
  **Combined header walk: ~250-400 ns**, similar to valkey.
- **Δ**: rough parity — both ~250-400 ns; kevy redundantly walks the
  header twice but each walk is short.
- **Attack candidate**:
  - **B1 (`uring_io.rs:184`, `uring_bigbulk_probe.rs:179-240`)** — the
    parser inside `dispatch_batch` already determines the frame is
    incomplete; pass that information forward to the BigBulk probe so
    it doesn't re-walk the header. Gain: ~80-150 ns/op. Semantic:
    drop-in refactor of the probe API to take a parser hint. Blast:
    ~60 LOC.

### S05 — slab → frame Vec memcpy (MEMCPY #1 — kevy-only, the main delta)

- **Reference (valkey)**: **does not exist**. The kernel write hits
  the eventual value sds directly.
- **Us (kevy)**: across 4 CQEs the BigBulk path executes 4 ×
  `Vec::extend_from_slice` on `state.frame` from kernel-filled slabs:
  - `uring_bigbulk.rs:87` (first hit, via `try_promote_bigbulk`):
    `frame.extend_from_slice(&tail[..16 404])`
  - `uring_bigbulk.rs:115` (subsequent hits, via `uring_bigbulk_feed`):
    3 × `state.frame.extend_from_slice(&slab[..take])`
  - Total bytes copied: 65 538.
- **Δ**: kevy pays **~3-7 µs/op of pure memcpy work here that valkey
  does not pay** (estimated 25 GB/s L2-hot down to ~10 GB/s when the
  source slab is just-arrived from kernel and dest frame is cold).
  Wall-time amortization at -c 50 sees a fraction of this as the gap.
- **Cause**: the BigBulk path was designed to eliminate the
  conn.input realloc storm; it succeeds at that but **does not eliminate
  the copy itself**. The slab is borrowed (provided-buffer ring owns
  it), so the bytes must move out before `pbuf.recycle(bid)`.
- **Attack candidate**:
  - **B2 (`uring_io.rs:118`, `uring_bigbulk.rs:104-125`,
    `kevy-uring/src/ring.rs::ProvidedBufRing`)** — defer slab recycle
    until the BigBulk frame completes; pass slab ownership into the
    Arc construction so the value bytes ARE the kernel-filled slab
    pages. Either (a) detach the slab from the ring and have its
    Arc::Drop return it to the ring's free list, or (b) use a
    dedicated big-arg slab class outside the multishot ring (sized to
    bulklen+2, single-shot recv into a fresh `Vec::with_capacity`,
    Arc-adopt on completion). Gain: removes the ~3-7 µs memcpy.
    Semantic: requires bench validation (ring back-pressure under
    sustained big-SET load; sliver-recycle semantics). Blast: 150+
    LOC across ProvidedBufRing + BigBulk.
  - **B2-alt** (cheaper / lower-risk): use option (b) — a dedicated
    single-shot recv into a `Vec::with_capacity(bulklen+2)` once the
    probe detects the shape. The Vec is sized exact, owned by kevy,
    and the recv writes the body bytes directly into it via the
    standard `read`-style SQE. Gain: same as B2. Blast: 80 LOC.

### S06 — frame → Arc memcpy (MEMCPY #2 — kevy-only)

- **Reference (valkey)**: **does not exist**. After parse, valkey
  adopts the querybuf sds as the value robj (`networking.c:3799
  createObject(OBJ_STRING, c->querybuf)`).
- **Us (kevy)**: after `dispatch_batch(cid, &frame)` parses argv from
  the assembled frame, `cmd_set` → `store.set_slice(&args[1],
  &args[2], …)` → `string.rs:38 Value::ArcBulk(Arc::from(bytes))`
  where `bytes = &frame[body_off..body_off+65 536]`.
  - `Arc::from(&[u8])` from libcore: allocates `Box<[u8]>` then memcpys
    the bytes. **65 536 B memcpy + ~200 ns alloc**.
- **Δ**: kevy pays another **~3-7 µs/op of memcpy** that valkey does
  not pay.
- **Cause**: `set_slice` takes a borrowed slice; the only correct way
  to land it into `Value::ArcBulk` is to copy into a fresh heap
  allocation. The path that previously avoided this (`set` taking an
  owned `Vec<u8>` + `pick_value_for_set_owned` at `string.rs:44-54`
  which adopts the Vec into Arc via `Vec::into_boxed_slice() →
  Arc::from(Box<[u8]>)`) is **available but not used by the BigBulk
  path** — the comment in `uring_bigbulk.rs:24-36` explicitly retired
  the v1.25 B.4 zero-copy adoption because that path used
  `self.store.set` which bypassed cross-shard routing.
- **Attack candidate**:
  - **B3 (`uring_bigbulk.rs:162-185 uring_apply_frame_stitch` +
    `string.rs:64 set`)** — restore Vec-adoption while preserving
    cross-shard routing. The fix is not to bypass routing but to make
    the cross-shard `Inbound::RequestBatch` path carry an owned
    `Vec<u8>` for the value bulk, then have the OWNING shard's
    `cmd_set` consume the Vec via `store.set(key, owned_vec, …)`
    (which already exists at `string.rs:64` and uses
    `pick_value_for_set_owned` → `Arc::from(Box)` zero-copy
    adoption). For the local-shard fast path (same-shard SET) the Vec
    can flow inline into `cmd_set`. Gain: removes the 64 KiB Arc
    memcpy. Semantic: requires plumbing an "owned value bulk" variant
    through `ArgvBorrowed` / `dispatch_batch` / `cmd_set` signature;
    requires bench validation. Blast: 200+ LOC, multi-crate (kevy-resp
    + kevy-rt + kevy + kevy-store). **The biggest architecturally
    clean attack**.
  - **B3-narrow** (small-blast variant): keep `store.set_slice` API
    unchanged but special-case the BigBulk path inside
    `uring_apply_frame_stitch` — extract the body slice's `(start, end)`
    relative to the frame Vec, split the Vec at `start`, drop the
    header prefix, truncate to body length, adopt the resulting Vec
    via `Vec::into_boxed_slice() → Arc::from(Box)`, then call a NEW
    `store.set_owned_arc(&key, Arc<[u8]>, …)` API. Bypasses
    `pick_value_for_set` int-detection (64 KiB body will never canonical-
    i64 anyway). Gain: same as B3. Blast: 80 LOC. Trade-off: bypasses
    cross-shard routing for big-SETs unless `--threads 1` or the key
    happens to hash to the owning shard — same constraint that
    retired the v1.25 B.4 path; needs the routing plumbing OR an
    explicit `--threads 1` workload assumption.

### S07 — header parse on the re-dispatched frame

- **Reference (valkey)**: header is parsed once in S04 and the bulks
  are walked incrementally as data arrives. No re-parse.
- **Us (kevy)**: `uring_apply_frame_stitch` calls `dispatch_batch(cid,
  &frame)` which runs the parser AGAIN over the assembled frame
  (`uring_bigbulk.rs:168`). The parser walks the header
  (`*3 $3 SET …`) and produces argv ranges. Per op: ~150-250 ns.
- **Δ**: kevy pays **~150-250 ns/op** of redundant parser work.
- **Cause**: the BigBulk probe could in principle hand back the
  pre-computed argv ranges and skip the second parse, but the design
  chose simplicity (re-dispatch is identical to the regular path).
- **Attack candidate**:
  - **B4 (`uring_bigbulk_probe.rs`, `uring_bigbulk.rs:162-185`)** —
    have the probe emit the argv ranges as part of `BigArgGenericProbe::Promote`; `uring_apply_frame_stitch` constructs a
    pre-parsed `ArgvBorrowed` and goes straight to `handle_command`
    bypassing the re-parse. Gain: ~150-250 ns/op. Semantic: requires
    bench validation (the borrowed argv ranges into `frame` must
    survive until the handler completes). Blast: ~100 LOC.

### S08 — verb dispatch / command lookup

- **Reference (valkey)**: `processCommand` →
  `commandCheckExistence` → `c->cmd` lookup via the parsed-cmd cache
  (~25 ns).
- **Us (kevy)**: `exec.rs:21 self.commands.resolve(args)` walks the
  verb-dispatch table; `exec_dispatch.rs:127` matches the fast path
  for `eq_ascii_get(name)` (3-byte verb). Per op: ~25-40 ns.
- **Δ**: parity.

### S09 — Store::set_slice (write the keyspace)

- **Reference (valkey)**: `setGenericCommand` → `setKey` → kvstore
  insert (or overwrite). The val robj's sds is the adopted querybuf
  from S04 — no new memcpy. Per op: ~150-250 ns (kvstore hash +
  bucket write + ref counting).
- **Us (kevy)**: `cmd_data.rs:205` → `string.rs:88 set_slice` →
  `string.rs:38 pick_value_for_set` (this is where memcpy #2 happens,
  accounted in S06) → `string.rs:107 set_value_no_evict` (F1 v1.25
  single-probe raw-entry-mut). KevyMap insert/overwrite: ~50-80 ns.
- **Δ**: kevy is structurally faster at the keyspace insert level
  (~80-150 ns), but the gain is dwarfed by the S06 Arc-memcpy paid in
  the same call.

### S10 — reply: write `+OK\r\n` (5 bytes)

- **Reference (valkey)**: `addReply(c, shared.ok)` →
  `_addReplyToBufferOrList` writes 5 bytes into `c->buf` inline.
  Per op: ~25-40 ns.
- **Us (kevy)**: `cmd_data.rs:206 encode_simple_string(out, "OK")`
  writes 5 bytes into `conn.output`. Per op: ~15-25 ns.
- **Δ**: parity (kevy slightly faster).

### S11 — pending-write queueing

- Per op: ~10-25 ns on both sides; parity (covered identically to the
  c100-GET decomp's S11).

### S12 — write submission (per iter, not per op)

- **Reference (valkey)**: `handleClientsWithPendingWrites` → `writeToClient` → `_writeToClient` → one `write()` syscall for 5
  bytes. Per op: ~50-80 ns userspace + ~1.0-1.5 µs kernel write.
- **Us (kevy)**: `uring_arm.rs:177-187` — `output_arcs` is empty (SET
  reply doesn't push any arc), so the simple `prep_write` branch
  fires: build an SQE pointing at `uc.write_buf` (5 bytes), submit on
  `submit_and_wait(0)`. Per op: ~30-60 ns userspace + ~1.0-1.5 µs
  kernel write. **The probe's H2 hypothesis (iovec chain assembly
  cost) does not apply to SET — the writev path is bypassed for
  SET's 5-byte reply.**
- **Δ**: kevy is structurally ahead on userspace by ~20-40 ns; kernel
  parity.

### S13 — kernel loopback TCP TX (server → client)

- Identical kernel path on both sides. Per op: ~1.0-1.5 µs for a 5-byte
  reply. **Probe's H3 (TCP loopback MSS / multi-send) doesn't apply to
  SET — the 5-byte reply fits in a single TCP segment.**

### S14 — write completion

- **Reference (valkey)**: `postWriteToClient` advances stats. ~30-50 ns.
- **Us (kevy)**: `uring_io.rs:362-372` — advance `write_off`, clear
  `write_buf` on full drain. ~30-60 ns.
- **Δ**: parity.

### S15 — per-iter overhead × iters per op

- **Reference (valkey)**: 50 conns / 1 main thread = 100% scheduling
  density when work present. `beforeSleep` per-iter cost ~200-400 ns
  amortized across N=20-50 events/iter at -c 50. Per op: ~10-15 ns.
- **Us (kevy)**: 50 conns / 2 shards = 25 conns/shard. At
  -P 1 with each conn pumping 1 SET per RTT (~700 µs), each shard
  sees ~25 productive iters per ms vs ~1000 busy-poll iters per ms.
  Per-iter overhead ~80-150 ns × idle multiplier. Per op (amortized
  over busy + idle iters): **~1-3 µs** of pure per-iter tax per op.
- **Δ**: kevy pays **~1-3 µs/op of per-iter overhead** that valkey
  does not. **This is the same finding as the c100-GET decomp**, and
  applies symmetrically to all kevy workloads at low conn-density
  per shard. Note this stage's tax shows up on **every** workload —
  so the **SET-specific gap (1.28 µs/op) less the GET-specific gap
  (0.40 µs/op) = 0.88 µs/op** isolates the SET-specific extra work
  (memcpys S05 + S06). The 0.40 µs GET gap is plausibly this stage
  plus iovec assembly (S12 reply for GET, not SET).

### S16 — accept / conn setup / amortized cold paths

- Per op amortized: ~0.2 ns/op. Parity.

### S17 — TTL clock refresh

- kevy refreshes `Store::cached_ns` once per CQE batch
  (`uring_reactor.rs:183-185`). For SET without TTL the clock is also
  consumed by `set_value_no_evict` via `(uc, cn) = (self.cached_clock,
  self.cached_ns)` (`string.rs:179`), but it's just a field read
  (~2 ns) since SET-NX/XX-without-expire doesn't compare clocks.
  Per op: ~10-30 ns. Same as the c100-GET decomp's S17.

### S18 — cross-shard routing for SET

- **Reference (valkey)**: N/A (single-threaded).
- **Us (kevy)**: at `--threads 2` the connection is bound to one of
  2 shards via SO_REUSEPORT. The key (`key:NN` from
  redis-benchmark) hashes uniformly across shards via
  `cmd_resolve.rs::shard_of`. ~50% of SETs cross-shard at 2 shards.
  Per cross-shard SET: argv pool take + ring push + remote shard's
  `exec_op` + reply ring push back. **Critically, the `args[2]` body
  (64 KiB) is currently moved across via the inbound batch carry —
  one MORE memcpy if not borrowed**. Let me re-read: `exec_op.rs:82`
  copies via `c.push(b"MSET")` — these are short Vec pushes per arg.
  For a 64 KiB value crossing shards, the argv plumbing currently
  must copy the body into the cross-shard message (the borrowed
  slice into `frame` can't escape the conn shard's stack).
- **Δ**: at `--threads 2`, ~50% of SETs pay an extra 64 KiB memcpy
  to cross shards. At a hot-cache 25 GB/s, that's ~2.6 µs × 50% =
  1.3 µs/op AVERAGED, on top of S06. **This number is suspiciously
  close to the entire measured 1.28 µs/op gap and is FLAGGED as a
  potential mis-estimate** — needs verification via runtime counter
  (see §C below).
- **Attack candidate**:
  - **B5 (cross-shard owned-value plumbing)** — same fix as B3 from
    the local-shard side. The cross-shard `Inbound::RequestBatch`
    must carry an owned `Vec<u8>` (or `Arc<[u8]>`) for the big-value
    bulk so the owning shard can adopt zero-copy. Combined with B3,
    this is one architectural change. Blast: subsumed by B3.

### S19 — slow-path housekeeping / AOF / replication

- All gated off in the bench config (no AOF, no replication). Per op:
  ~10-15 ns. Parity.

---

## §C — Runtime-verification flags (per methodology §2)

Several stage estimates above are **source-only predictions** and need
runtime counter confirmation before Phase B begins. Per methodology
§2 "luna fib_28 lesson", source-only is necessary but not sufficient.
Flag for runtime verification:

1. **S05 actual byte volume of slab→frame memcpys per SET**. The probe
   path is correct but the count per op depends on how the multishot
   recv CQEs slice the 65 KiB body. Verify via a counter on
   `uring_bigbulk::uring_bigbulk_feed` calls and accumulated bytes.
   - Quick check: `cargo run --example diag_bigarg -- -d 65536 -n 1000`
     and `eprintln!("bigbulk_feed bytes={}", state.frame.len() - prev);`.
2. **S06 actually fires per SET**. Confirm that `pick_value_for_set`
   takes the `Value::ArcBulk` arm for the bench's 64 KiB body
   (it should — body is random ASCII so `parse_canonical_i64` fails
   and `BULK_THRESHOLD=64` is far below 64 KiB). Quick check: a
   counter increment in the `bytes.len() > BULK_THRESHOLD` arm at
   `string.rs:37` reported per second.
3. **S18 fraction of SETs that go cross-shard at `--threads 2`**.
   The "50%" estimate assumes uniform hashing; redis-benchmark's
   `key:NN` keyspace may bias. Quick check: counter on
   `exec_dispatch.rs:93` for the cross-shard branch vs local-shard,
   reported as a ratio.
4. **S15 idle multiplier**. The c100-GET decomp predicted ~80 idle
   iters per productive iter at sparse conn density. At -c 50 with
   --threads 2, the multiplier should be ~3-5 (much denser per
   shard). Quick check: counters on
   `Shard::run_uring` for total iters and "did_work iters" per second,
   reported as a ratio.

If S05/S06 confirmed and S18 confirmed at ~50%, the predicted total
extra SET work is ~5-12 µs of memcpy per op (mostly Phase B targets
B2 + B3). The observed 1.28 µs/op gap suggests the memcpys are L2-warm
(closer to 25 GB/s than 10 GB/s) and the kernel TCP RX side overlaps
heavily with the userspace work, masking some of the cost.

---

## §D — Cross-cutting overhead

### D01 — io_uring_enter syscall amortization

Same finding as the c100-GET decomp: `ENTER_SKIP_THRESHOLD=2` forces a
syscall every other iter even on idle iters, costing ~150-450 ns/op
at -c 50 across two shards. Attack A14 from the c100-GET decomp
applies identically here; it is NOT specific to big-SET workload.

### D02 — provided-buffer ring pressure at sustained big-SET load

The pbuf ring is sized `4096 × 16 KiB = 64 MiB` (`uring_reactor.rs:52`).
A 64 KiB SET consumes ~4 slabs per op; at 63 k SET/s × 2 shards that's
~500 k slab-acquires/s per shard — well within the ring's recycle
throughput. However, **B2-alt (dedicated single-shot recv for big-arg)
would bypass the provided-buffer ring entirely for big-SETs**, and
that's actually a feature: the ring is sized for many small conns; big
SETs ad-hoc-allocate a Vec sized to bulklen+2 outside the ring,
avoiding pressure on the small-payload path.

### D03 — frame Vec heap allocations

For each big SET, kevy heap-allocates a fresh `Vec::with_capacity(65 568)` at
`uring_bigbulk.rs:86`. ~250 ns alloc + ~150 ns dealloc per op =
~400 ns/op of malloc traffic. valkey's path also allocates fresh
sds at `networking.c:3804 sdsnewlen(SDS_NOINIT, c->bulklen + 2)`,
so parity here.

### D04 — Arc heap allocation (the destination for memcpy #2)

`Arc::from(&[u8])` for 64 KiB body: `Box<[u8]>` allocation (~150-300 ns
for a 64 KiB jemalloc small slab — possibly more if it falls into the
mmap path). ~200 ns/op. valkey's `createObject(OBJ_STRING, c->querybuf)`
is a much smaller alloc (just the robj header) since the sds is
adopted. **kevy pays ~150-250 ns/op extra alloc traffic** for the Arc
that doesn't need to exist on the valkey side.

This stage is **subsumed by B3** — eliminating the Arc memcpy also
eliminates the Arc alloc (we'd adopt the existing frame Vec's heap
allocation into the Arc via `Vec::into_boxed_slice() → Arc::from(Box)`).

---

## §E — Budget validation

**Reference (valkey)** per-op productive CPU at -c 50 -d 65536:

| Stage | ns/op |
|---|---|
| S01 (RX kernel, amortized across ~4 chunks) | 6 000 |
| S03 (4 × read syscall, ~6 µs each on first arrival) | (kernel-overlapped; counted in S01) |
| S04 (parseMultibulkBuffer + big-arg sds resize) | 350 |
| S08 (cmd lookup) | 25 |
| S09 (setKey, kvstore insert/overwrite + sds adoption) | 200 |
| S10 (addReply 5 bytes) | 30 |
| S11 (pending-write queue) | 12 |
| S12 (writev 5 bytes + kernel half) | 1 500 |
| S14 (postWrite) | 40 |
| S15 (per-iter overhead, amortized) | 12 |
| everything else (S02/S05/S06/S07/S13/S16-S19) | 60 |
| **Total productive CPU** | **~8 230 ns** |

Wall-time per op at 69 252 ops/s = 1/69 252 × 10⁹ ≈ 14 440 ns. Budget
sum / wall-time = 0.57 — **off by 43%**, exceeds the ±20% bar. The
discrepancy is dominated by S01 (kernel TCP RX 6 µs) which is
heavily parallel with userspace work in a 50-way pipeline; counting it
as "per-op productive CPU" double-counts. Subtracting half of S01 as
kernel-side parallel work: ~5 230 ns → wall ratio 0.36, still off.

**This means the valkey-side stage tabulation needs a real perf-record
to reconcile**. The c100-GET decomp also failed this bar (per its
"Budget validation" section, Reference sum exceeded budget by 2.6×).
The methodology §2 bar is genuinely hard to clear without a profile
from the matching workload — flagged for Phase B prerequisite.

**Us (kevy)** per-op productive CPU at -c 50 -d 65536 (measured
63 613 ops/s = 15 717 ns/op):

| Stage | ns/op |
|---|---|
| S01 (RX kernel) | 6 000 |
| S03 (4 × CQE pickup) | 30 |
| S04 (dispatch_batch first walk + bigbulk probe) | 350 |
| **S05 (slab → frame memcpy, 4 × ~16 KiB)** | **3 500-7 500** |
| S07 (re-dispatch parse over frame) | 200 |
| S08 (cmd lookup) | 30 |
| **S06 (Arc::from 64 KiB body memcpy)** | **3 500-7 500** |
| S09 (KevyMap insert/overwrite) | 70 |
| S10 (encode 5 bytes) | 20 |
| S11 (mark_arm_pending) | 25 |
| S12 (prep_write 5 bytes + kernel half) | 1 200 |
| S14 (uring_on_write) | 40 |
| S15 (per-iter overhead) | 1 500 |
| D04 (Arc alloc) | 200 |
| S18 (cross-shard hop, 50% probability × ~3 000 ns) | 1 500 |
| everything else (S02/S13/S16/S17/S19) | 100 |
| **Total productive CPU** | **~18 270 - 26 270 ns** |

Wall-time 15 717 ns / sum mean ~22 270 ns → ratio 0.71. Off by 29% —
**still misses the ±20% bar but closer than the valkey side**. The
S05+S06 memcpy bands span ~7-15 µs, dominating the budget; the actual
realised cost depends heavily on cache state which the source-only
read cannot pin down without a real perf-record at `-d 65536`.

**Decomposition is INCOMPLETE per methodology §2**. The structural
finding (kevy has 2 × 64 KiB memcpys that valkey lacks) is solid; the
per-op µs apportionment between S05/S06/S18 needs the runtime
verification from §C before Phase B prioritisation.

---

## §F — Hypothesis verdict (against the probe's H1/H2/H3)

**H1 — `Conn::output` buffer growth strategy doubles unnecessarily**:
**REFUTED for SET, partially relevant for GET.** SET's reply is 5
bytes, far below any growth boundary; `conn.output` does not grow.
The probe's framing was reply-side, but SET's bottleneck is inbound.
The relevant inbound-side variant (`conn.input` realloc storm) was
already addressed in v1.25 B.4 (`uring_bigbulk.rs:86 Vec::with_capacity(total)`) — frame Vec is pre-sized,
no realloc storm.

**H2 — io_uring iovec chain assembly cost at 64 KiB**: **REFUTED for
SET.** SET reply uses `prep_write` (single buffer, not writev) because
`output_arcs` is empty (`uring_arm.rs:177-187`). No iovec assembly
happens for SET reply. The H2 framing applies to **GET reply** path
(where `output_arcs` carries the Arc-bulk for writev), and that's
where the 0.40 µs/op GET gap may partially originate — but **GET's
gap is not this decomp's target**.

**H3 — TCP loopback MSS / multi-send for 64 KiB write**: **REFUTED for
SET.** SET reply is 5 bytes; fits in a single TCP segment. No
multi-send. Probe's H3 applies to GET reply.

**H4 — NEW finding (not on probe's list)**: **CONFIRMED at source level
(needs runtime verification per §C)**. The 64 KiB SET pays **two
userspace memcpys of the value body that valkey avoids**:

1. **slab → frame** (`uring_bigbulk.rs:87 + :115`,
   `extend_from_slice` per CQE) — 65 538 B.
2. **frame → Arc** (`string.rs:38 Arc::from(bytes)` invoked from
   `pick_value_for_set`) — 65 536 B.

Valkey avoids both by:

1. **read() syscall writes kernel data straight into the eventual sds**
   (sized to bulklen+2 at `networking.c:4243-4254 sdsMakeRoomForNonGreedy`).
2. **`createObject(OBJ_STRING, c->querybuf)` adopts the sds as the
   value robj** (`networking.c:3799`), zero-copy.

Combined kevy overhead per 64 KiB SET: **131 KiB of extra userspace
memcpy + ~200 ns extra Arc alloc**. At ~25 GB/s L2-hot effective
memcpy: ~5.2 µs/op of extra CPU. The measured 1.28 µs/op gap is
consistent with substantial pipeline overlap masking the full cost.

**H5 — companion finding**: the cross-shard plumbing for SET at
`--threads ≥ 2` may add an extra 64 KiB body memcpy on the ~50% of
SETs whose key hashes to a different shard from the conn's owning
shard (S18 above). This is a kevy-architecture-specific tax that
exists because the bench keyspace `key:NN` hashes uniformly across
shards. **Needs runtime verification** of the cross-shard fraction
before sizing the attack.

---

## §G — Top-N actionable attacks (sorted by µs gain, big-SET-specific)

| # | File:line | Change | Gain µs/op | Semantic | Blast |
|---|---|---|---|---|---|
| B3 | `uring_bigbulk.rs:162-185 uring_apply_frame_stitch` + `kevy-resp/src/argv_borrowed.rs` + `kevy/src/dispatch.rs` + `kevy/src/cmd_data.rs:205` + `kevy-store/src/string.rs:64` + cross-shard `Inbound::RequestBatch` plumbing | Plumb an owned-Vec value-bulk variant through ArgvBorrowed → cmd_set → store.set, including the cross-shard Inbound message. cmd_set consumes the Vec via `store.set(key, owned_vec, …)` which uses `pick_value_for_set_owned` → `Arc::from(Box<[u8]>)` zero-copy adoption. Eliminates memcpy #2 (S06) and ALSO covers the cross-shard hop (S18) | ~3-7 µs/op (S06) + up to 1.5 µs/op (S18) | requires bench validation; multi-crate refactor with strict ownership lifetime checks | 200+ LOC |
| B2-alt | `uring_io.rs:118-131` + `uring_bigbulk.rs:66-125` + new `kevy-uring` API for single-shot read into owned Vec | When `probe_generic_bigbulk` returns Promote: cancel the multishot recv on this conn, allocate `Vec::with_capacity(bulklen+2)`, submit a single-shot `prep_read(fd, vec.as_mut_ptr() + 0, bulklen+2)` SQE. CQE delivers the bytes directly into the Vec; no slab involvement, no slab→frame memcpy. Re-arm multishot after the SET completes | ~3-7 µs/op (S05) | requires bench validation (multishot cancel/rearm semantics under in-flight CQEs); needs `kevy-uring` Read SQE op support (likely already there) | 150 LOC across kevy-rt + kevy-uring |
| B2 | (alternative to B2-alt) — slab ownership transfer | Detach the kernel-filled slab from the provided-buffer ring on big-arg promote; the slab pages become the Arc<[u8]> body bytes directly. Arc::Drop returns the slab to a free list | ~3-7 µs/op (S05) | requires major ProvidedBufRing refactor; back-pressure semantics under sustained big-SET load are subtle | 250+ LOC |
| B5 | (subsumed by B3) — cross-shard owned-value plumbing | Same fix as B3 from the cross-shard angle | (counted in B3) | (counted in B3) | (counted in B3) |
| B4 | `uring_bigbulk_probe.rs` (return argv ranges) + `uring_bigbulk.rs:162-185` | Probe emits argv ranges as part of Promote; `uring_apply_frame_stitch` skips the second parse pass and goes straight to `handle_command` | ~150-250 ns/op | requires bench validation (borrowed argv into `frame` lifetime) | 100 LOC |
| B1 | `uring_io.rs:184` + `uring_bigbulk_probe.rs` | Pass the partial-parse result from `dispatch_batch`'s incomplete frame into `probe_generic_bigbulk` so it doesn't re-walk the header | ~80-150 ns/op | refactor of probe API; no semantic change | 60 LOC |
| A14 | `kevy-uring/src/ring.rs:67 ENTER_SKIP_THRESHOLD` | Raise to 16 on idle iters; force enter on cross-shard / SQE submission. (Inherited from c100-GET decomp — applies to ALL workloads, not just big-SET) | ~150-450 ns/op | requires bench validation | 30 LOC |

**Total ceiling** for big-SET specifically (B3 + B4 + B1, no overlap):
~3.5-8 µs/op. This bracket exceeds the measured 1.28 µs/op gap by
3-6× — consistent with substantial pipeline-overlap masking, but
**also a signal that the §E budget validation gap is real and B3
alone may close more than the visible wire gap, putting kevy ahead at
big-SET too**.

**The big architectural attack is B3** — eliminating memcpy #2 by
plumbing owned-Vec value-bulks through the BigBulk frame-stitch
path and the cross-shard `Inbound::RequestBatch` carrier. The
v1.25 B.4 attempt at this was retired specifically because it
bypassed cross-shard routing; B3 is the same destination via a
correct route. **Combined with B2-alt to eliminate memcpy #1, kevy's
big-SET path becomes structurally equivalent to valkey's adopted-sds
path with 0 extra value-byte memcpys**.

---

## §H — What the probe missed (closing notes)

The axis-sweep probe's three hypotheses (H1/H2/H3) were all **reply-side
hypotheses derived from the framing "single large bytes leaving the
server"**. That framing matches `-d 65536` GET (server sends 64 KiB
reply) but **does not match `-d 65536` SET** (server receives 64 KiB
body and sends a 5-byte reply). The probe correctly identified the
losing axis but then routed all 3 hypotheses to the wrong code path.

The actual SET bottleneck is **inbound-side userspace memcpys** —
two of them, totalling 131 KiB per SET — that valkey avoids via:

1. Pre-sizing the recv buffer to bulklen+2 so `read()` lands the
   bytes directly in the eventual sds (sds = querybuf = future value
   payload).
2. Adopting the sds as the value robj on completion (`createObject(OBJ_STRING, c->querybuf)`).

kevy's BigBulk frame-stitch path (shipped v1.25 B.4 + B.5) eliminated
the conn.input realloc storm but did not eliminate the underlying
copies — it just moved them around (slab→frame instead of slab→input,
and the frame→Arc copy was always there). The original v1.25 B.4
zero-copy adoption (Vec::into_boxed_slice → Arc::from(Box)) was on the
right track but was retired because it bypassed cross-shard routing.
The correct fix is to **make cross-shard routing carry owned Vecs**
(B3), not to bypass routing.

**Confidence**: high that B3 closes the 8% SET gap on `--threads ≥ 2`;
medium that B2-alt closes additional headroom (depends on cache
state which source read can't fully resolve). Low confidence on
B4/B1 individually moving the needle but they're cheap and reduce
parser overhead identified separately. The runtime verifications in
§C should run BEFORE any attack lands — particularly the S18
cross-shard fraction (which controls whether B3's S18 component is
worth its plumbing cost) and the S05/S06 actual byte volumes per op
(which control the order of attack-priority between B2-alt and B3).

---

*Phase A read-only decomposition complete. Phase B (attack
implementation, worktree-isolated) is a separate future task. Top
priority for Phase B: §C runtime verifications first; then **B3 + B2-alt
as the architectural pair** to bring kevy's big-SET path to valkey's
zero-memcpy posture. The local-shard fast path within B3 also handles
the `--threads 1` workload (which is how multi-shard Lua / ecosystem
tests now run, per v1.27.x ship history).*

---

## §I — Runtime verification via perf record (2026-06-28, post-decomp)

§C flagged 4 source-only stage estimates needing runtime counters. The
most decisive of them — whether the predicted 2 memcpys actually show
up in CPU% under load — can be answered by perf record alone, no
counter patches needed. Did that.

### Setup

- Host: lx64 16-core, mitigations=off (from session 8), kernel 6.12.
- kevy: `/root/kevy/target/release/kevy --threads 2`, taskset 0-1.
- valkey: 9.1.0 `--io-threads 10 --io-threads-do-reads yes`, taskset 0-9.
- Bench: `redis-benchmark -c 50 -P 1 -d 65536 -n 1.5M -t set`,
  taskset 10-13. Warmup 50k, then perf record over 12s while bench
  ran at steady state.
- Sampling: `perf record -F 999 --call-graph fp -p <pid>`.

### Results

**kevy top symbols (self time, no children):**

| % self | symbol |
|---|---|
| 16.31% | `rep_movs_alternative` (kernel — TCP recv/send copy) |
| 15.92% | `libc.so.6 0x162e47` (= `__memcpy_avx_unaligned_erms`, userspace memcpy) |
| 9.54%  | unresolved `kevy 0x6c56c` (release binary, no debug symbols) |
| 2.88%  | `nft_do_chain` (kernel netfilter — same as 2026-06-20 finding) |
| <1% each | misc syscall/kernel symbols |

**valkey top symbols (self time, no children, summed across threads):**

| % self | symbol |
|---|---|
| 6.71% | `rep_movs_alternative` (kernel, summed io_thd_1 4.90% + io_thd_2 1.81%) |
| 4.94% | `libc.so.6 0x162e47` (userspace memcpy, summed io_thd_1 2.14% + io_thd_2 0.80% + main 2.00%) |
| 5.71% | `getMonotonicUs_x86` (clock reads — valkey-specific) |
| 3.61% | `beforeSleep` |
| 2.49-2.40% | `pthread_mutex_lock` (io threads coordinating) |
| 2.49% | `spmcDequeue` (io thread work queue) |

### Verdict on §C runtime flags

**S05+S06 (the 2 extra userspace memcpys) — CONFIRMED.**

- kevy userspace memcpy = **15.92%** of CPU at -d 65536 SET
- valkey userspace memcpy = **4.94%** of CPU at the same workload
- **kevy spends ~3.2× more userspace CPU on memcpy than valkey** — directly visible in perf record. Maps to the source-level finding of 2 extra memcpys (slab→frame at uring_bigbulk.rs:87/115, frame→Arc at string.rs:38).

**Kernel rep_movs (TCP copy) — caveat:**

- kevy 16.31% kernel rep_movs > valkey 6.71% kernel rep_movs.
- A part of this delta is thread-count amortization: valkey has 10 io threads parallelizing the kernel-copy work, kevy has 2. Per-thread the gap shrinks.
- But not fully — kevy's per-thread kernel copy share (16.31% / 2 threads = 8.16% per kevy thread) is still higher than valkey's per-io-thread share (4.90%/2 = 2.45% per io thread). So there's a real per-conn-read extra kernel-copy in kevy too, likely the same memcpy path going through the provided-buffer recv mechanism.

**Unresolved 9.54% kevy symbol at `0x6c56c`:**

- Release binary lacks debug symbols → addr2line lookup deferred. Likely candidates per the decomp doc: `Shard::uring_drain_inbound` body, `bigbulk_feed`, or `Arc::from(bytes)`. Rebuild with `[profile.release-perf] debug = "line-tables-only"` (matches the 2026-06-20 setup) gives addr2line on this hop.

### What this changes for Phase B

The structural finding (2 extra memcpys) is now both source-confirmed AND runtime-confirmed. Phase B priority order unchanged:

1. **B3** — owned-Vec value-bulks plumbing. Highest confidence, biggest gain.
2. **B2-alt** — single-shot prep_read with sized buffer. Additive on top of B3 OR alternative if B3 too invasive.

Sizing estimate: closing the 11pp userspace-memcpy gap (15.92% → ~5%) corresponds to ~10-11% throughput recovery at this workload. The measured gap is 8%; recovering 10-11pp suggests B2-alt + B3 not only closes the gap but **inverts the lead** to ~2-3%. That's the upper bound; actual lower due to other constraints (e.g. heap allocator pressure on freed buffers, cache disturbance from the new path).

### §C remaining flags — unverified

- **S18 cross-shard 50% guess** — not verified. Needs counter patch; perf can't see it directly. Defer to first commit on the Phase B attack branch.
- **S15 idle multiplier at --threads 2** — same. Defer.

These don't gate Phase B start; they refine the µs apportionment but not the attack target.
