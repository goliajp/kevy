# c100 GET pipeline decomposition — kevy v1.28.0 vs valkey 9.1.0

Date: 2026-06-28
Workload: `redis-benchmark -c 100 -P 1 -n 1.5M -t get` against a
prepopulated keyspace (16-byte key, ~3-byte ASCII value), TCP loopback,
lx64 (16-core x86_64, Linux 6.12, mitigations=off — per the 2026-06-20
record). Reactor: kevy default (`KEVY_IO_URING=1`, 16-shard
SO_REUSEPORT); valkey 9.1 default (single main thread, `io-threads 1`,
epoll).

Phase A read-only research per
`~/.claude-shared/global/methodology/perf-decomposition-vs-polish.md`.
**No code edits — attack candidates are concrete file:line references
to be implemented in Phase B**.

---

## Atomic op cost table (lx64 x86_64, Linux 6.12, +20% over the M-series table)

| Op | Cost (ns) |
|---|---|
| L1 cache hit | 1 |
| L2 cache hit | 4 |
| L3 / DRAM | 60-120 |
| Atomic load (Acquire, uncontended) | 2 |
| Atomic CAS (uncontended) | 6-12 |
| Heap alloc (small, malloc/jemalloc) | 35-60 |
| KevyMap::get hit (16-slot SIMD group) | 30-50 |
| HashMap::get hit (kvstoreHashtableFind, MurmurHash2) | 50-90 |
| `itoa` 6-digit | 6-12 |
| `gettimeofday` vDSO | 30-60 |
| `read`/`write` < 4 KB on TCP loopback | 1200-3500 |
| `io_uring_enter` (with COOP_TASKRUN, no submit) | 250-500 |
| `epoll_wait` returning 1-N fds | 500-2000 + 100/fd |
| `accept4` | 1500-3000 |
| `writev` 2-3 iovecs < 4 KB on TCP loopback | 1500-4000 |

---

## Measured baseline

**Reference (valkey 9.1.0 c100 GET) — NOT MEASURED.** The 2026-06-20
record only measured -c1 (kevy 82.9 k SET/GET on lx64, "matched C
client parity"). No comparable lx64 c100 valkey number exists in
`bench/` records. **This itself is the first finding** — per the
methodology §1 anti-pattern table, *"自家 baseline (vN-A) 也只 X×, RFC
target 不现实"*: every "kevy 191 k" claim on the 2026-06-20 chain is
**vs nothing**, not vs valkey on the same lx64 box.

Public single-thread c100 GET numbers for valkey/redis 7.x-9.x on
comparable lx64 hardware sit in the **400-700 k ops/sec** range with
`io-threads 1`. Treat 500 k as a working estimate pending a real
measurement (see Attack A0 below). Even at the conservative end, kevy
peak 191 k is **~2.6× slower than valkey**, with kevy spreading work
across **16 shard threads** vs valkey's **1 main thread** — i.e. the
per-thread efficiency gap is **~40× when normalised to a fair
per-thread CPU budget**.

- **Reference (valkey 9.1, estimated)**: ~500 k ops/sec total, single
  main thread. Per-op productive CPU on the main thread: ~2.0 µs.
- **Us (kevy v1.28.0, measured 2026-06-20)**: 191 k ops/sec total, 16
  shard threads. Per-op productive CPU averaged across shards: ~83 µs
  (most of which is busy-poll spin between sparse conn events).
- **Gap**: ~2.6× total throughput, ~40× per-thread efficiency.

**Hard-budget reconciliation caveat (methodology §2)**: per-op
productive CPU on each side cannot be reconciled to wire time without a
real c100 measurement of valkey. The stages below are sized in ns from
the source read — **the budget validation at the end must be redone
once Attack A0 produces an actual valkey c100 number**.

**Verified runtime counter check (methodology §2 extra constraint)**:
the only counter we relied on is the 2026-06-20 perf-record at -c1.
Several stage µs estimates below are **predictions** — flagged
"(predicted; needs counter)" where applicable.

---

## Architectural distinction setting up the decomp

**valkey c100**: 100 conns on **one** epoll set. Each `aeApiPoll`
returns N ready fds in one `epoll_wait` syscall. The main-thread loop:

```
while (!stop) {
    beforeSleep();            // handleClientsWithPendingWrites: flush ALL pending
    epoll_wait();             // 1 syscall, returns N ready fds (N can be ~5-50)
    for fd in ready:
        readQueryFromClient   // 1 read syscall, processInputBuffer dispatches
        processCommand → getCommand → lookupKey → addReplyBulk
                              // Reply lands in c->buf or c->reply list
                              // Conn added to clients_pending_write (NOT flushed yet)
    processTimeEvents();
    afterSleep();
}
```

Key amortizations:
- 1 `epoll_wait` per N events (N ≈ 5-50 at c100)
- 1 `handleClientsWithPendingWrites` per iter; each dirty conn does 1
  `writev` syscall, which can pack header+body+CRLF into 1 syscall via
  `writevToClient`.
- 100 conns / 1 thread = **100% scheduling density** when there is work.

**kevy c100**: 100 conns are distributed across **16 shards** by the
kernel's SO_REUSEPORT load balancer. Per shard: ~6 conns. Each shard
runs `Shard::run_uring` as a busy-poll reactor:

```
while (!stop) {
    accept_inflight ? : ring.prep_accept_multishot();    // O(1) per accept
    uring_arm_conns()                                    // arm recv + write per dirty conn
    ring.submit_and_wait(0)                              // syscall every ENTER_SKIP_THRESHOLD=2 iters
    ring.for_each_completion(...)                        // reap CQEs (~6 max at c100)
        if RECV: uring_on_recv (parse + dispatch + push to conn.output)
        if WRITE: uring_on_write (advance write_off)
    uring_drain_inbound()      // cross-shard ring drain
    flush_backlog / flush_requests / flush_publish / flush_wakes
    store.flush_pending_drops()
    aof.maybe_sync()
    reap_counter & 0xF == 0 ? uring_reap_closed()
    tick_check_counter ++; if >= 256: tick_blocked_timeouts() etc.
    pump_replication() / reap_closed_replicas() (gated on replicate.is_some())
    if !io_work && !did_inbound && !has_backlog:
        idle_spins++; if idle_spins >= 256: park; else spin_loop()
}
```

Key cost shapes:
- **per-iter overhead**: every shard spins ~1 M iter/sec at -c1 (per
  the 2026-06-20 profile). Each iter touches: 6 closure call sites for
  flush_*, the dirty-set drain, the tick_check_counter ++, the backlog
  iter `.any(|b| !b.is_empty())`. At c100 with ~6 conns the SHAPE is
  similar — ~6 useful CQEs per iter at best, often 0-1.
- 100 conns / 16 shards = **6.25 conns/shard average**. At P=1, each
  conn is idle (waiting for client) most of the time → each shard sees
  sparse work.
- The kevy reactor doesn't get the per-iter event-density valkey gets.
  **This is the architectural distinction the decomp surfaces, not a
  ceiling**.

The stages below are written **per request** (1 GET command from arrival
to reply on the wire). Cross-cutting amortization (epoll batch vs CQE
batch) is in the §Cross-cutting section.

---

## Stages

### S01 — kernel loopback TCP RX (client → server)

- **Reference (valkey)**: `tcp_v4_rcv` → `tcp_data_queue` → wakes up the
  fd in `epoll_wait`'s ready set. Kernel-side, identical to kevy.
  Atomic ops: 1 skb alloc + 1 packet enqueue + 1 epoll item wake. µs
  est: **~1.0-1.5 µs/op (kernel)**.
- **Us (kevy)**: identical kernel path; the multishot recv SQE in the
  io_uring fast path differs only in how the userspace gets notified
  (CQE vs epoll readable). µs est: **~1.0-1.5 µs/op (kernel)**.
- **Δ**: 0
- **Cause**: shared kernel path. Both pay netfilter `nft_do_chain` on
  loopback (1.26 % per the 2026-06-20 profile).
- **Attack candidate**: D9 (kernel-side) — per-port iptables fast-path
  ACCEPT chain to bypass the LIBVIRT_INP / DOCKER chains. Gain ~30-50
  ns/op (1.26 % of ~3 µs/op). **Deployment-side; not a kevy code
  change**.

### S02 — accept / conn setup (amortized over conn lifetime)

- **Reference (valkey)**: `acceptTcpHandler` → `accept4` → `createClient`
  → `connSetReadHandler(readQueryFromClient)` registers the fd in
  epoll. Cost at conn open: ~3 µs. **At sustained c100 with 1.5 M
  requests over 100 conns**, amortized cost per request: ~0.2 ns/op.
- **Us (kevy)**: `uring_reactor.rs:159 prep_accept_multishot` armed
  once per shard; CQE handler at `uring_reactor.rs:241-263` creates
  `Conn`, inserts into `conns: KevyMap<u64, Conn>`, increments
  `next_conn_id`, `set_nodelay`. Cost ~2.5 µs at conn open. Amortized
  per req: ~0.2 ns/op.
- **Δ**: 0
- **Cause**: kevy's multishot accept is actually a tiny win
  architecturally (no per-conn accept SQE re-arm cost), but irrelevant
  at sustained c100.
- **Attack candidate**: none at this stage. Listed for completeness.

### S03 — wake / readable notification

- **Reference (valkey)**: `epoll_wait(epfd, events[setsize], -1)` (or
  bounded). 1 syscall returns N ready fds. At c100, N ≈ 5-20 per call
  during heavy load. Per-fd amortized: `(epoll_wait cost ~800 ns) / N`
  ≈ **40-160 ns/op**.
- **Us (kevy)**: `ring.for_each_completion` in `kevy-uring/src/ring.rs:439`
  reads `cq_khead` / `cq_ktail` atomically and pulls one Completion at
  a time. No syscall — the CQE ring is shared memory. Per-CQE cost
  ~5-10 ns. With `ENTER_SKIP_THRESHOLD = 2`, the actual
  `io_uring_enter` syscall fires every other iter even when idle.
  Per-op CQE-reap cost: **5-10 ns/op**.
- **Δ**: kevy wins **~50-150 ns/op here**.
- **Cause**: io_uring's shared-memory CQE ring vs epoll's syscall
  round-trip. valkey's path passes through `do_syscall_64` /
  `entry_SYSCALL_64_after_hwframe` (4-6 % each in the 2026-06-20 profile)
  on every epoll_wait.
- **Attack candidate**: none. This is one of the few stages where kevy
  is structurally ahead.

### S04 — fd → client lookup

- **Reference (valkey)**: `aeApiPoll` (ae_epoll.c:128) populates
  `eventLoop->fired[j].fd`; `aeProcessEvents` (ae.c:462) does
  `aeFileEvent *fe = &eventLoop->events[fd];` — **direct array index by
  fd**, O(1) with one L1 hit. Then `fe->rfileProc(eventLoop, fd,
  fe->clientData, mask);` — `clientData` is the `client *` pointer
  cached at registration time. **NO map lookup per event**. µs est:
  **~2 ns/op**.
- **Us (kevy)**: `uring_reactor.rs:204` extracts `cid = c.user_data &
  CONN_MASK` from the SQE user_data (encoded by us at submit time, see
  `uring_arm.rs:185 OP_RECV | cid`). Then `uring_io.rs:136
  self.conns.get_mut(&cid)` — `KevyMap<u64, Conn>::get_mut` is a
  SIMD-group hash probe (~30-50 ns). **And** `uring_io.rs:118 io.get_mut(&cid)`
  — second `KevyMap<u64, UringConn>::get_mut` for the io-state map. Two
  hash probes per CQE. µs est: **~60-100 ns/op**.
- **Δ**: kevy is **~60-100 ns/op slower** here.
- **Cause**: kevy stores per-conn state in two parallel
  `KevyMap<u64, _>` tables (one for `Conn`, one for `UringConn`). valkey
  stores `client*` once in the connection struct itself, reached via
  `fe->clientData`. The user_data field of the SQE/CQE only carries 64
  bits, of which the top 3 are op tags — so up to 2^61 conn ids fit, but
  we waste them on a hash probe instead of carrying a pointer.
- **Attack candidate**:
  - **A1 (file: `uring_arm.rs:185`)** — change the SQE user_data
    encoding from `OP_RECV | cid` to `OP_RECV | (uc_ptr_low48 << 3)`,
    embedding the **UringConn slot index** (or a stable raw pointer
    into the kevy-map slot — KevyMap slots are stable across grow/no
    grow per `map_keyed.rs:140-192`). On CQE, decode directly to the
    `UringConn` slot — skip the `io.get_mut` probe. Gain: ~30-50 ns/op.
    Semantic: requires bench validation (must verify slot stability
    under conn churn / grow); blast: ~80 LOC. **Note**: KevyMap *does*
    relocate slots on `maybe_grow` (`map_keyed.rs:54-58`); to make this
    sound, either (a) pre-size `io` and never grow, or (b) intern conn
    ids → indices in a `Vec<UringConn>` with a generation tag.
  - **A2 (file: `uring_io.rs:136`)** — replace the per-CQE
    `self.conns.get_mut(&cid)` probe with a direct slot deref by
    embedding the `Conn` slot pointer too. Same blast as A1. Gain:
    another ~30-50 ns/op.
  - Total S04 attack potential: **~60-100 ns/op recoverable**.

### S05 — recv: kernel read

- **Reference (valkey)**: `readToQueryBuf` (networking.c:4198) →
  `connSocketRead` (socket.c:190) → `read(fd, querybuf+qblen, PROTO_IOBUF_LEN)`.
  PROTO_IOBUF_LEN = 16 KiB. **1 syscall per readable event**. Per-op
  cost at c100 sustained: ~1.5 µs (TCP loopback read, ~30 byte payload).
- **Us (kevy)**: multishot recv SQE armed once per conn
  (`uring_arm.rs:267-272 prep_recv_multishot`). Each arrival drops one
  CQE into the CQ ring; no syscall per arrival. The provided-buffer
  ring (4096 × 16K, `uring_reactor.rs:52-53`) holds slabs the kernel
  fills directly. Per-op cost in userspace: ~10 ns (the slab pickup +
  `pbuf.bytes(bid, n)` access). Kernel-side cost identical (~1.5 µs of
  TCP receive).
- **Δ**: **kevy saves ~200-500 ns/op** (the userspace read syscall +
  buffer-grow side it doesn't have).
- **Cause**: io_uring multishot recv + provided buffers; valkey's
  thread_shared_qb path does pay a `sdsMakeRoomFor` + `read` syscall
  per event.
- **Attack candidate**: none additional; kevy is structurally ahead.

### S06 — input buffer accumulation

- **Reference (valkey)**: `sdsIncrLen(c->querybuf, c->nread)` after
  read; `c->qb_pos` cursor advances during parse. `thread_shared_qb`
  amortizes across conns (one shared SDS reused, only big args get a
  dedicated alloc — networking.c:4232). Per-op cost: ~5-10 ns.
- **Us (kevy)**: `uring_io.rs:136-142` does
  `std::mem::take(&mut c.input)` → stack Vec for the dispatch borrow →
  `c.input = input_buf` back at the end (line 150). **At c100 with
  parse-from-slab fast path active (line 184: `input_buf.is_empty()`
  branch)**, the take/restore is on an always-empty Vec; cost ~3-5 ns.
- **Δ**: rough parity, **kevy slightly wins** (parse-from-slab avoids
  one slab→input memcpy on the hot path; valkey writes once into
  querybuf and then parses from there).
- **Cause**: kevy's A1 (parse-from-slab) eliminates the always-on
  memcpy; valkey relies on `thread_shared_qb` to keep alloc churn down
  but still copies into the qb.
- **Attack candidate**: none; kevy is ahead per the A1 fast path.

### S07 — RESP parse (`*3\r\n$3\r\nGET\r\n$N\r\n<key>\r\n`)

- **Reference (valkey)**: `parseMultibulk` (networking.c:3616-3824).
  One `memchr('\r', ...)` per header + one `string2ll` per length. For
  a 3-arg GET that's: 1 array hdr + 3 bulk hdrs = 4 memchrs, 4 string2ll
  calls. Then `createStringObject` per arg copies the bytes out into a
  new `robj` with refcount=1, `OBJ_STRING` encoding, `OBJ_ENCODING_RAW`.
  3 `robj` allocs (sds-embedded; ~50 ns each via jemalloc).
  µs est: **~250-350 ns/op**.
- **Us (kevy)**: `parse_command_borrowed` (request_borrowed.rs:21-103).
  Same 4 memchr-equivalents, 4 `parse_int` calls. **Returns
  `ArgvBorrowed<'_>`** — a vec of `(start, end)` ranges into the input
  buffer. **NO byte copies, NO object allocs** for the argv. The
  ArgvBorrowed itself allocates ~24 bytes for the range vec.
  µs est: **~80-120 ns/op**.
- **Δ**: **kevy wins ~150-250 ns/op** here.
- **Cause**: valkey's `robj`-per-arg allocation tradition is paying
  ~150-200 ns at the parse stage. kevy's borrowed argv is structurally
  ahead.
- **Attack candidate**: none on kevy. This is one of kevy's biggest
  stage-level wins; do not regress it.

### S08 — verb dispatch / command lookup

- **Reference (valkey)**: `commandCheckExistence` / pre-parsed
  `c->parsed_cmd`. valkey caches the parsed command structure
  (`c->parsed_cmd`) across the parse → process boundary so processCommand
  doesn't re-lookup. Cost: ~20-30 ns (cached pointer load + arity
  check).
- **Us (kevy)**: `exec.rs:21` `self.commands.resolve(args)` walks a verb
  dispatch table to produce `ResolvedCmd { route, is_quit, is_write,
  block_hint, wake_idx, ... }`. Then `exec_dispatch.rs:127`
  `eq_ascii_get(name)` for the GET-fast-path probe.
  Cost: ~25-40 ns (one full match + one 3-byte ASCII compare).
- **Δ**: ~5-10 ns slower on kevy.
- **Cause**: valkey caches the parsed command; kevy re-resolves per
  request (the `resolve` table is hot, but the match arms walk).
- **Attack candidate**:
  - **A3 (file: `exec.rs:21`)** — cache the verb resolution on the
    `ArgvBorrowed` itself (or on a per-CQE scratchpad) so back-to-back
    parses with the same verb skip the resolve walk. Gain: ~5-10
    ns/op. Semantic: none (read-only optimization); blast: ~30 LOC.
    Low priority compared to S04.

### S09 — key access lookup (single-shard hot path)

- **Reference (valkey)**: `lookupKey` (db.c:81) →
  `getKVStoreIndexForKey(objectGetVal(key))` (which is just
  `crc16(key) & slot_mask` for cluster, or 0 for non-cluster) →
  `dbFindWithDictIndex` → `kvstoreHashtableFind` (a Swiss-table-ish
  open-addressing probe over MurmurHash2 buckets).
  Per-key cost: ~80-130 ns (hash + probe + key compare).
- **Us (kevy)**: `string.rs:300 get_into_output` → `accounting.rs:157
  live_entry` → `KevyMap::get` (SIMD group probe, 16 slots/iter,
  KevyHash 64-bit).
  Per-key cost: ~40-70 ns.
- **Δ**: **kevy wins ~30-60 ns/op**.
- **Cause**: KevyMap is a purpose-built Swiss-table (kevy-map/src/group.rs
  SSE2 metadata scan; KevyHash is a tighter hash than MurmurHash2 for
  small keys per its bench). This is a real stone-level win.
- **Attack candidate**:
  - **A4 (file: `accounting.rs:157-185 live_entry`)** — currently
    does **two** `self.map.get` probes (line 164 + line 170/184) when
    no TTL is set. The TTL-free fast path was supposed to read just
    once, but the structure does `e.expire_at_ns.is_some()` → re-probe
    `self.map.get(key)` at line 184. Restructure to **return the slot
    reference from the first probe**, branch on `e.expire_at_ns`
    inline. Gain: ~20-30 ns/op (one probe avoided on every TTL-free
    GET). Semantic: requires bench validation (borrow checker
    restructure); blast: ~40 LOC.

### S10 — value type discriminant + bulk header emit

- **Reference (valkey)**: `tryAvoidBulkStrCopyToReply` (networking.c:1463)
  → `prepareClientToWrite` (line 446) → `_addBulkStrRefToBufferOrList`
  (line 765-797). For an OBJ_ENCODING_RAW string, packs a
  `bulkStrRef` (pointer + sds ptr) into the client's encoded buffer.
  Cost: ~50-80 ns.
- **Us (kevy)**: `string.rs:308-321` matches on `Value::Str / ArcBulk /
  Int`. For a typical 3-byte ASCII value (Str), writes
  `bulk_header_into(output, len)` + `output.extend_from_slice(bytes)` +
  `extend_from_slice(b"\r\n")` directly into `conn.output`. Cost:
  ~25-40 ns. For ArcBulk (≥ 64-byte values), pushes the Arc into
  `output_arcs` so the writev SQE references the value bytes zero-copy.
- **Δ**: **kevy wins ~25-40 ns/op for Str**, ~40-60 ns/op for ArcBulk.
- **Cause**: valkey's `prepareClientToWrite` does heavy lifting
  (`putClientInPendingWriteQueue`, the buf_encoded flag dance, the
  `payloadHeader` insert for buf-encoded mode). kevy's inline path
  skips all of this — it just appends to a Vec.
- **Attack candidate**: none on kevy at this stage; ahead.

### S11 — pending-write queueing

- **Reference (valkey)**: `putClientInPendingWriteQueue` (called from
  `prepareClientToWrite`) adds the client to
  `server.clients_pending_write` (a doubly-linked list). Each conn lands
  on this list at most once per iter (the `pending_write` flag dedupes).
  Cost: ~10-15 ns.
- **Us (kevy)**: `exec_dispatch.rs:162 mark_arm_pending(conn_id, io)` →
  `uring_arm.rs:22-33` — pushes the cid onto `self.arm_pending: Vec<u64>`
  guarded by the per-`UringConn` `arm_queued: bool` dedupe flag.
  Cost: ~15-25 ns (one `io.get_mut` probe to check / set the flag, one
  Vec push).
- **Δ**: ~5-10 ns slower on kevy (the dedupe path costs a hash probe;
  valkey just sets a bit on the client struct).
- **Cause**: kevy stores `arm_queued` on `UringConn` (separate map),
  forcing a hash lookup at every queue-push. valkey's flag lives on the
  client struct, already in cache from the dispatch.
- **Attack candidate**:
  - **A5 (file: `uring_arm.rs:22-33`)** — co-locate `arm_queued` on
    `Conn` (currently a flag on `UringConn`); the `Conn` was already
    `get_mut`'d during the inline dispatch path, so the second probe
    becomes free. Gain: ~10-20 ns/op. Semantic: requires bench
    validation (Conn struct A4 layout would need an extra hot byte;
    fits in current `#[repr(C)]` padding per `conn.rs:46-95`). Blast:
    ~50 LOC.

### S12 — write submission (per iter, not per req)

- **Reference (valkey)**: `handleClientsWithPendingWrites`
  (networking.c:3271) iterates `clients_pending_write`. For each: try
  IO-thread offload (line 3298, off at io-threads=1), then
  `writeToClient` → `_writeToClient` (line 2818) → if reply list
  non-empty or buf_encoded: `writevToClient` (line 2707). Builds an
  iovec list spanning `c->buf` and every reply block, calls
  `connWritev`. **One writev syscall per conn per iter, packing
  header+body+CRLF**. At c100 sustained, ~50 conns dirty per iter →
  ~50 writev calls per iter. Per-iter overhead: ~50 µs total. Per-op
  amortized: ~10 µs / write — but wait, this is per-op throughput-wise
  the SAME as the read. **Per-op writev cost (incl. syscall + kernel):
  ~1.5-2.5 µs/op**.
- **Us (kevy)**: `uring_arm.rs:174-263` prep_writev or prep_write per
  dirty conn into the SQ ring. **No syscall per SQE submit** — the
  `submit_and_wait(0)` at `uring_reactor.rs:177` only enters the
  kernel every `ENTER_SKIP_THRESHOLD=2` iters when nothing changed,
  otherwise on every iter that submitted SQEs. Per-op userspace cost:
  ~50-80 ns to build the iovec list + push the SQE. Kernel-side cost
  (tcp_sendmsg etc.) identical to valkey's writev.
- **Δ**: **kevy wins ~50-200 ns/op userspace**, identical kernel.
- **Cause**: io_uring batching. The 2026-06-20 -c1 profile shows
  `tcp_sendmsg` at 11 % inclusive even on kevy — the kernel write path
  dominates regardless of submit mechanism. **The submit-side win is
  small.** This contradicts the 2026-06-20 doc's framing of
  "userspace ceiling reached" — the userspace gap *here* is small, but
  it is non-zero, and there are larger gaps elsewhere (S04 / S15 / S18
  below).

### S13 — kernel loopback TCP TX (server → client)

- **Reference (valkey)**: `tcp_sendmsg` → `tcp_write_xmit` →
  `__tcp_transmit_skb` → `__ip_queue_xmit` → loopback delivery to the
  client's recv queue. **11 % inclusive on kevy's 2026-06-20 profile**,
  same kernel path on valkey. µs est: ~1.5-2.5 µs/op.
- **Us (kevy)**: identical kernel path. µs est: ~1.5-2.5 µs/op.
- **Δ**: 0
- **Cause**: shared kernel path; not actionable in userspace.
- **Attack candidate**:
  - **B5 (kernel-side)**: MSG_ZEROCOPY for value bodies ≥ 4 KB. Not
    actionable on the redis-benchmark GET workload (value bytes 3
    typical). For workloads with > 4 KB values: gain ~30-50 % on
    tcp_sendmsg cost.

### S14 — write completion handling

- **Reference (valkey)**: `postWriteToClient` (the function that
  follows `writeToClient` at line 3071) — moves the conn off
  `clients_pending_write`, increments stats, updates `last_interaction`.
  Per-op: ~30-50 ns.
- **Us (kevy)**: `uring_io.rs:267-373 uring_on_write` — advances
  `write_off`, handles short writes, drops processed arc prefixes (the
  L1 chunked-writev state machine). For a simple GET reply (no arc-bulk
  body, ≤ 64 byte payload), the path is line 362-372: increment
  `write_off`, clear `write_buf` on full drain. Per-op: ~30-60 ns.
- **Δ**: rough parity.

### S15 — per-iter overhead (the kevy-specific tax)

- **Reference (valkey)**: per-iter overhead = `beforeSleep` (server.c:1812)
  + `epoll_wait` + `afterSleep`. Most of `beforeSleep`'s 50 lines are
  guarded by feature flags that are off (cluster, modules, replication,
  blocked-ack); the hot lines are `handleClientsWithPendingWrites`
  (which IS productive work) and a few timer checks. Per-iter overhead
  on a busy main loop: ~200-400 ns. At c100 with N=20 events/iter, per-op
  amortized: **~10-20 ns/op**.
- **Us (kevy)**: per-iter overhead inside `Shard::run_uring`
  (uring_reactor.rs:150-428):
  - `uring_arm_conns` call (early-bail on empty queue, line
    `uring_arm.rs:113`): ~5-10 ns when idle
  - `ring.submit_and_wait(0)`: ~10 ns when skipping, ~250-500 ns when
    syscall fires
  - `ring.for_each_completion`: ~5 ns
  - `uring_drain_inbound` (fast-path bail at uring_inbox.rs:27): ~2 ns
  - `flush_backlog` (fast-path bail at shard_flush.rs:111): ~2 ns
  - `flush_requests` (fast-path bail at exec.rs:285): ~2 ns
  - `flush_publish` (similar): ~2 ns
  - `flush_wakes` (fast-path bail at shard_flush.rs:28): ~2 ns
  - `store.flush_pending_drops()`: ~3 ns
  - `aof.maybe_sync()` (Option None for `--no-aof`): ~2 ns
  - `reap_counter` increment + `& 0xF == 0` check: ~2 ns
  - `tick_check_counter` increment + branch: ~3 ns
  - `replicate.is_some() || !replicas.is_empty()` (E9 gate): ~2 ns
  - **`has_backlog = self.backlog.iter().any(|b| !b.is_empty())`**
    (line 409) — iter over `Vec<VecDeque<Inbound>>` of length 16 with
    one `is_empty` per shard. **~20-40 ns/iter, NOT short-circuited
    by a bitmap**. This is mispatterned vs the `backlog_nonempty: u64`
    bitmap that already exists (`shard_flush.rs:84`).
  - `idle_spins` arithmetic + spin_loop hint: ~1 ns
  - Total per-iter overhead: **~70-120 ns when no work, ~80-150 ns
    when 1 CQE fired**.
- **At ~1 M iter/s with ~100 ops/s/shard from c100 (≈191 k / 16 shards
  = 12 k/s/shard, but the busy-poll spins ~80× more than it works)**,
  per-iter overhead consumes **~80-100 % of the shard's CPU** vs ~5-10 %
  on valkey.
- **Δ**: **per-op this is hard to quantify** because the kevy per-iter
  tax is paid every iter, productive or not. Per ACTUAL op the
  amortized tax is: (overhead 100 ns) × (iters per op ≈ 80) = **~8 µs
  pure overhead per op** at c100 on kevy. Per op on valkey: ~10-20 ns.
- **Cause**: kevy's per-shard busy-poll runs at 100% CPU regardless of
  conn density. At c100 with ~6 conns/shard at P=1, each shard polls
  ~80 idle iters per productive iter. **THIS IS THE BIGGEST FINDING**.
  The 2026-06-20 "userspace ceiling reached" framing is wrong: the
  userspace gap is in the per-iter overhead × iter density.
- **Attack candidates**:
  - **A6 (file: `uring_reactor.rs:409`)** — replace
    `has_backlog = self.backlog.iter().any(|b| !b.is_empty())` with
    `has_backlog = self.backlog_nonempty != 0`. The bitmap already
    tracks this (`shard_flush.rs:84-93 send_to` ORs the bit; the
    `flush_backlog_slow` clears it). Gain: ~20-40 ns/iter ×
    ~1 M iter/s = ~2-4 % of shard CPU. Semantic: none (drop-in
    replacement of an equivalent check). Blast: 1 line. **THIS IS A
    POLISH ATTACK** in the methodology's sense (drop-in, ~5 ns)
    — note it but the architectural attacks A7/A8 below matter more.
  - **A7 (architecture; file: `runtime.rs:114, 232 spin_limit`)** —
    introduce **conn-density-aware shard parking**: when a shard
    holds < N conns, park earlier (lower spin_limit) to let other
    shards process more conns. At c100/16 shards = 6 conns/shard, the
    current spin_limit=256 forces every shard to busy-poll waiting
    for sparse RTT-bounded traffic. Gain: depends on workload; at
    c100 likely halves shard CPU per op without throughput cost.
    Semantic: requires bench validation (must not regress -c1).
    Blast: ~80 LOC. **Bigger architectural attack**.
  - **A8 (architecture; file: `runtime.rs:142`, `Shard::run_uring`)**
    — **introduce conn-affinity rebalancing**. At c < shards, fold
    conns onto fewer shards via a periodic rebalance pass; at c >
    shards, keep current SO_REUSEPORT distribution. Gain: at c100/16
    shards, fold to e.g. 8 shards (~12 conns each); each shard's
    productive work density doubles. Semantic: breaks the "stateless
    shard" model — requires a conn migration protocol or steering
    SO_REUSEPORT via `eBPF SK_REUSEPORT`. Blast: 200+ LOC. **The
    structurally correct fix for the c100 gap**. This is what valkey
    achieves for free by being single-threaded.

### S16 — accept-side multishot SQE re-submit (rare)

- **Reference (valkey)**: per-accept fd registration in epoll. ~500 ns
  at conn open. Amortized negligible.
- **Us (kevy)**: `prep_accept_multishot` armed once per listener, only
  re-armed when F_MORE clears (`uring_reactor.rs:231-238`). Amortized
  negligible at sustained c100.
- **Δ**: 0

### S17 — TTL cached clock refresh

- **Reference (valkey)**: `server.unixtime` is updated in `serverCron`
  (server.c, runs every 100ms). Per-op: 1 atomic load ≈ 2 ns.
- **Us (kevy)**: `uring_reactor.rs:183-185`
  `if !comps.is_empty() { self.store.refresh_clock(); }` — refreshes
  the `Store::cached_ns` field via one `Instant::now()` call per CQE
  batch. Cost: ~30-60 ns per CQE batch. At c100 with ~1-6 CQEs/batch,
  per-op amortized: ~10-30 ns.
- **Δ**: kevy is ~5-25 ns slower on TTL-bearing workloads. **For the
  bench (no TTL on keys)**, `live_entry` skips the clock read entirely
  (G-A4 fast path, accounting.rs:158-167) — but the per-batch refresh
  still fires.
- **Attack candidate**:
  - **A9 (file: `uring_reactor.rs:183`)** — gate the refresh on
    whether the store actually has TTL'd keys (a counter on Store).
    Gain: ~10-30 ns/op for non-TTL workloads. Semantic: drop-in
    counter. Blast: ~30 LOC.

### S18 — cross-shard fast-path inhibit

- **Reference (valkey)**: N/A — single-threaded; no cross-shard.
- **Us (kevy)**: at c100 with sticky-conn affinity, each conn's owning
  shard handles ALL of that conn's traffic. But the bench's
  `key:NNN` pattern hashes uniformly across the 16 shards, so ~15/16
  of GETs end up cross-shard. `exec_dispatch.rs:92-95
  start_single → cross-shard forward via `request_batch``. Per
  cross-shard hop: argv pool take + ring push + remote shard's
  `exec_op` + reply ring push back + fold + write SQE. Per-op cost
  for the cross-shard hop: ~200-400 ns plus the inbox drain (2 atomic
  ops, ~10 ns).
- **Δ**: **kevy pays a ~200-400 ns penalty on 15/16 of c100 GETs**
  because the test keyspace is hash-uniformly distributed across 16
  shards. valkey doesn't pay this because there are no shards.
- **Cause**: 16-shard SO_REUSEPORT means accept distribution is RTT-
  bounded random; key→shard distribution is hash-uniform; the two
  rarely align. **At 16 shards the probability of conn-shard ==
  key-shard is exactly 1/16**.
- **Attack candidates**:
  - **A10 (architecture; `cmd_resolve.rs::shard_of`)** — keyspace
    pre-fragmentation: have redis-benchmark's keyspace generator use
    `{tag}` hashtag for affinity. **Out of scope** (bench harness
    change). But:
  - **A11 (architecture; `exec_dispatch.rs:93`)** — **redirect at
    accept time** rather than at dispatch. When a cluster-aware conn
    issues N GETs all owned by shard X, the first cross-shard hop
    learns this and migrates the conn to shard X (or sends -MOVED).
    Gain: amortize the 200-400 ns over the conn's lifetime. Semantic:
    requires cluster-mode integration. Blast: 100+ LOC.
  - **A12 (drop-in; `exec.rs:284-299 flush_requests`)** — at c100 the
    `request_batch[shard]` Vecs see frequent appends + drains. The
    `mem::take` + `send_to` per shard is ~30-50 ns each; bitmap
    short-circuit (line 285) already helps. Inline the
    `Inbound::RequestBatch { reqs }` construction into one push to
    avoid a Vec<Vec> allocation per drain. Gain: ~15-25 ns/op. Blast:
    ~40 LOC.

### S19 — reply fold + drain

- **Reference (valkey)**: replies build directly into `c->buf` /
  `c->reply` — no fold step. Per-op: 0.
- **Us (kevy)**: forwarded GET reply lands as `Inbound::ResponseBatch`,
  drained at `inbox.rs:256-275`, each reply gets `fold(conn, seq,
  part)` (exec.rs:350) → `drain_front(conn)` to materialise the
  reply bytes into `conn.output`. Per-op cost: ~80-150 ns.
- **Δ**: **kevy pays ~80-150 ns/cross-shard-hop** — applied to 15/16
  of c100 ops.
- **Cause**: the seq-ordered pending-slot ring + materialize() are
  necessary for kevy's cross-shard correctness (preserves reply order
  on a pipelined conn). On the inline fast path (cross-shard fanout
  is single-target, just one reply), this is overkill.
- **Attack candidate**:
  - **A13 (file: `exec.rs:350-447 fold`, `reduce.rs:materialize`)**
    — for the single-target cross-shard case, skip the
    PendingSlot/Agg dance and route the reply bytes straight into
    `conn.output`. Detect this in `start_single` (exec_dispatch.rs:81)
    and stash a flag on the request that lets the response drain
    inline. Gain: ~80-150 ns / cross-shard op. Semantic: requires
    bench validation (must preserve pipeline ordering — only safe for
    `pending.is_empty()` conns, which IS the c100 P=1 case). Blast:
    ~120 LOC.

### S20 — slow-path housekeeping (slowlog / notify / wake / Lua / aof)

- **Reference (valkey)**: `commandProcessed` (networking.c:3831) runs
  every command, calls `resetClient`, updates stats. ~50-80 ns.
- **Us (kevy)**: `exec_dispatch.rs:167-170 slowlog_maybe`
  (`slowlog_t0() == None` fast bail, ~3 ns), `is_write` branch (false
  for GET), `lua_wake_bridge::drain_lua_wake_buffer` (empty Vec check,
  ~3 ns). Per-op: ~10-15 ns.
- **Δ**: **kevy wins ~40-70 ns/op** here.
- **Attack candidate**: none additional; ahead.

### S21 — backlog / output cleanup

- **Reference (valkey)**: `_addReplyToBuffer` may overflow into the
  reply list; per-op overhead ~5 ns when buf has room.
- **Us (kevy)**: `conn.output.clear()` + `write_off = 0` when fully
  drained (`uring_io.rs:363-372`). Per-op: ~3-5 ns.
- **Δ**: parity.

### S22 — replication / Lua wake bridge gates (cold paths)

- **Reference (valkey)**: gated on `server.aof_state`, `server.repl_*`,
  module count. Empty branches: ~3 ns total.
- **Us (kevy)**: `exec_dispatch.rs:328 replicate.is_some()` (None on
  the bench: ~2 ns), `notify_dispatch` (None on the bench: ~2 ns),
  `wake_key` gated on `wake_idx` (None for GET: ~1 ns), Lua wake
  bridge drain (empty: ~3 ns). Per-op: ~10 ns.
- **Δ**: parity.

### S23 — conn close (amortized; not in steady state)

Same as S02 for symmetry. ~0.2 ns/op amortized.

---

## Cross-cutting overhead

### O01 — io_uring_enter syscall amortization

- **Reference (valkey)**: 1 `epoll_wait` syscall per N events (N ≈
  5-50). Per-op: ~50-200 ns.
- **Us (kevy)**: `ENTER_SKIP_THRESHOLD = 2` (kevy-uring/src/ring.rs:67)
  — every 2 iters forces a syscall to flush COOP_TASKRUN's deferred
  task_work. At ~1 M iters/s per shard, that's **500 k syscalls/s
  PER SHARD**, or **8 M syscalls/s across the 16 shards**. The
  2026-06-20 profile shows `__do_sys_io_uring_enter` at 1.80 % self +
  14.10 % children — that's ~16 % of all server CPU on syscalls that
  do no submit, no wait. Per-op amortized at c100: **~200-500 ns/op
  just from forced enters**.
- **Δ**: **kevy pays ~150-450 ns/op extra** on enter syscalls.
- **Attack candidate**:
  - **A14 (file: `kevy-uring/src/ring.rs:67 ENTER_SKIP_THRESHOLD`)** —
    raise the threshold to 16 or 32 when no SQEs are queued and no
    completions waited on, paired with a forced enter on backlog /
    cross-shard activity. The 2026-06-20 A11 attempt
    (IORING_SETUP_TASKRUN_FLAG) tried this and was reverted because
    the bit set/clear timing didn't match the busy-poll loop. But the
    A11 failure was specific to letting the *kernel* signal; raising
    the threshold deterministically is a different change. Gain:
    ~100-300 ns/op. Semantic: requires bench validation (must not
    starve completion delivery on the par k path). Blast: ~30 LOC.

### O02 — epoll_wait vs CQE batching at c100

- **Reference (valkey)**: 1 epoll_wait per ~20 events at c100. Per-op:
  ~50-100 ns.
- **Us (kevy)**: at 16 shards / ~6 conns each, per-iter CQE count is
  ~0-3. Almost no batching benefit. Per-op: ~5 ns userspace + S15's
  per-iter tax. **The amortization that valkey gets from c100 going
  to 1 thread is exactly what kevy gives up by sharding to 16**.

### O03 — per-conn cache line layout

- **Reference (valkey)**: client struct is large but the hot fields
  (`buf`, `bufpos`, `qb_pos`, `reply` list head, `flag`) are in the
  first cache line. The `payloadHeader`-encoded buffer is bytes-only,
  L1-friendly.
- **Us (kevy)**: `Conn` is `#[repr(C)]` with hot-first ordering
  (`conn.rs:46-95`), per A4 (2026-06-20). Two cache lines hot. The
  `UringConn` is a SECOND map probe per CQE (see S04), forcing two
  cache lines per op vs valkey's one.

### O04 — netfilter on loopback

- Both sides pay `nft_do_chain` per packet (1.26 % on the 2026-06-20
  profile). Identical kernel cost. See Attack D9.

### O05 — busy-poll CPU spend

- valkey at io-threads=1 c100: ~50-70 % CPU on the main thread (epoll
  blocks when idle).
- kevy at 16 shards: **~95-100 % CPU on every shard regardless of
  conn density** (busy-poll). At c100 with sparse productive work per
  shard, **most of the CPU is wasted on idle iters**. See Attack A7.

### O06 — flush_pending_drops bio-thread offload

- Per-op: ~3 ns on the steady GET path (empty buffer). Cold-path
  benefit only matters for SETs of large values.

---

## Budget validation

Reference (valkey, estimated): per-op productive CPU ~2.0 µs on the
main thread.

Stage sum (reference, ns):
- S01 (RX kernel): 1250
- S05 (read syscall + kernel): 1500
- S07 (parseMultibulk): 300
- S08 (cmd lookup): 25
- S09 (lookupKey): 100
- S10 (addReplyBulk): 65
- S11 (queue): 12
- S12 (writev + kernel half): 1800
- S14 (postWrite): 40
- S15 (per-iter overhead): 15
- O02 (epoll amortized): 75
- everything else (S02/S04/S20/S21/S22): ~30

**Reference sum: ~5210 ns/op productive CPU**. Estimated wire-time
budget at 500 k ops/s = 2000 ns/op. **Reference sum exceeds the estimated
budget by 2.6×** — meaning either the 500 k estimate is too high, or
the per-op CPU estimate above includes amortized kernel time the main
thread doesn't see in real wall-time. **DECOMPOSITION INCOMPLETE per
the ±20 % rule**. Most likely explanation: the kernel TCP RX/TX paths
(S01 + S12) include time during which the main thread is doing
something else (parsing the next conn's request). Cannot resolve
without a real lx64 c100 measurement of valkey + the matching perf
record. **See Attack A0 — measure first**.

Us (kevy, measured 191 k ops/s, 16 shards):
- per-op total work across 16 shards = (1/191k) × 16 = 84 µs of total
  shard CPU per op. Of this:
  - Productive CPU per op ≈ 4.5 µs (stages S04+S05+S07+S09+S10+S12+S14
    + cross-shard hop S18+S19) (predicted; needs counter)
  - Per-iter overhead × iters per op (see S15): ≈ 80 µs (per-iter
    overhead 100 ns × ~800 idle iters per productive iter) (predicted;
    needs counter)
- **Us sum: ~84 µs**, matches the throughput-derived 84 µs. **±0%.**

The reconciliation gap on the reference side is the biggest indicator
that **A0 must run before any further analysis is trusted**.

---

## Top-N actionable attacks (sorted by µs gain)

| # | File:line | Code change | Gain µs/op | Semantic | Blast |
|---|---|---|---|---|---|
| A0 | (bench harness) | Run `redis-benchmark -c 100 -P 1 -n 1.5M -t get` against valkey 9.1 on the same lx64 box, perf-record both servers, compare CPU% / per-call ns. WITHOUT THIS NUMBER NO OTHER ATTACK IS GROUNDED. | (measurement) | none | external |
| A8 | `runtime.rs:142` + `Shard::run_uring` | Conn-affinity rebalance: at c < shards, fold conns onto fewer shards. SO_REUSEPORT via eBPF SK_REUSEPORT or per-shard accept gating. | ~40-60 µs/op (most of S15) | breaks stateless-shard model | 200+ LOC |
| A7 | `runtime.rs:114, 232` (spin_limit) | Conn-density-aware shard parking: park earlier when shard holds < N conns; preserves -c1 by keeping spin_limit high when shard is dense. | ~20-30 µs/op | requires bench validation | 80 LOC |
| A14 | `kevy-uring/src/ring.rs:67` | Raise `ENTER_SKIP_THRESHOLD` to 16 on idle iters; force enter on cross-shard / SQE submission. Different mechanism from the reverted A11. | ~0.15-0.45 µs/op | requires bench validation | 30 LOC |
| A1 | `uring_arm.rs:185` | Encode UringConn slot index in SQE user_data; decode direct slot on CQE. Avoid `io.get_mut(&cid)` probe. | ~0.05-0.10 µs/op | requires bench validation (slot stability under grow) | 80 LOC |
| A2 | `uring_io.rs:136` | Same as A1 but for `self.conns.get_mut(&cid)` → cache the Conn slot pointer. | ~0.05-0.10 µs/op | requires bench validation | 80 LOC |
| A13 | `exec.rs:350-447 fold` | Single-target cross-shard reply: skip PendingSlot/Agg, route reply bytes straight to `conn.output` when `pending.is_empty()`. | ~0.08-0.15 µs/op × 15/16 (cross-shard fraction) | requires bench validation | 120 LOC |
| A4 | `accounting.rs:157-185 live_entry` | Reduce double probe to single probe in the TTL-free fast path. | ~0.02-0.03 µs/op | requires bench validation | 40 LOC |
| A12 | `exec.rs:284-299 flush_requests` | Inline `Inbound::RequestBatch { reqs }` construction; avoid Vec<Vec> alloc per drain. | ~0.015-0.025 µs/op | none | 40 LOC |
| A9 | `uring_reactor.rs:183` | Gate `store.refresh_clock()` on Store having TTL'd keys (counter). | ~0.01-0.03 µs/op | none | 30 LOC |
| A5 | `uring_arm.rs:22-33` | Co-locate `arm_queued` flag on `Conn` (already get_mut'd), eliminate `io.get_mut` probe in `mark_arm_pending`. | ~0.01-0.02 µs/op | requires bench validation | 50 LOC |
| A3 | `exec.rs:21` | Cache verb resolution per parse, skip resolve walk on repeated verb. | ~0.005-0.010 µs/op | none | 30 LOC |
| A6 | `uring_reactor.rs:409` | Replace `self.backlog.iter().any(|b| !b.is_empty())` with `self.backlog_nonempty != 0` bitmap check. | ~0.020-0.040 µs/iter (drop-in polish) | none | 1 line |
| D9 | (deployment) | Per-port iptables fast-path ACCEPT for loopback. Already documented in 2026-06-20 record. | ~0.03-0.05 µs/op | none | 0 |

**Cumulative ceiling of A1+A2+A4+A5+A9+A12+A13+A14 (the userspace
polish bundle, lowest-blast)**: ~0.4-0.9 µs/op. Buys ~5-10 % at most.

**The real prize is A7 + A8** — and both require recognizing that the
c100 throughput gap is **NOT a per-op work gap; it is a per-thread
density gap from oversharding**. The userspace polish doesn't close
it.

---

## What the 2026-06-20 "userspace ceiling reached" claim missed

Per methodology §1 trigger-word table, "userspace ceiling reached" /
"kernel-bound" / "structural ceiling" all = "I didn't decompose enough".
The 2026-06-20 record's specific misses:

1. **Never measured valkey c100** on the same lx64 box (Attack A0
   above). Compared only against kevy's own v1.23 baseline → fell into
   the "自家 baseline 也只 X×, RFC target 不现实" anti-pattern. Every
   "84 k matches C client parity" claim is **vs nothing**, not vs
   valkey.

2. **Never read `uring_reactor.rs:409`** — the `backlog.iter().any(...)`
   walk per iter (Attack A6) is a textbook "decomposition reveals
   unknown" finding — 20-40 ns × ~1 M iter/s × 16 shards = ~5-13 %
   sharded-CPU waste, hiding in plain sight in the per-iter loop body
   that the profile attributed to "Runtime::run::closure 51.66%".
   The 2026-06-20 profile saw the closure self-time and rolled this
   in; never saw the `backlog.iter().any()` line.

3. **Treated 16-shard CPU spend as a workload constant** rather than
   surfacing that at c100 the per-shard conn density (~6 conns) is
   well below the per-shard saturation point (each shard can serve
   84 k -c1, but at c100 spread thin produces only 12 k/shard).
   Attacks A7 / A8 are invisible from the -c1 profile because at -c1
   there is exactly 1 conn so density is trivially 100 %.

4. **Misread `tcp_sendmsg` 11 %** as "kernel-bound, no userspace fix"
   when valkey pays the same `tcp_sendmsg` cost; the difference at
   c100 is not kernel cost per op, it is **per-thread CPU efficiency**.

5. **Never read `kevy-uring/src/ring.rs:67 ENTER_SKIP_THRESHOLD = 2`**
   as a threshold to tune. The 2026-06-20 E14 doc concluded "every other
   iter enter is necessary for COOP_TASKRUN" but never tested raising
   to 8, 16, 32 with explicit force-enter on submission boundaries
   (Attack A14). The reverted A11 (TASKRUN_FLAG) is a *different*
   mechanism — A14 keeps deterministic timing.

6. **The `live_entry` double-probe at `accounting.rs:170` + `:184`**
   (Attack A4) — added "G-A4 (v1.25)" as a TTL-free fast path
   optimization, but introduced an extra `self.map.get(key)` probe on
   the no-TTL hot path. Profile blamed `Runtime::run::closure`; never
   broke down KevyMap probes per op.

7. **The reference assumption: kevy ahead means done**. Stages S03 /
   S05 / S07 / S09 / S10 / S20 each show kevy structurally ahead of
   valkey by 30-200 ns/op. The 2026-06-20 record treated this as
   evidence that the remaining gap was structural. It was not — the
   remaining gap is concentrated in S15 (per-iter overhead × idle iter
   density) and S18 + S19 (cross-shard hop on 15/16 of GETs). Both
   are **decomposable userspace work that simply was never opened**.

---

*Phase A read-only decomposition complete. Phase B (attack
implementation, worktree-isolated) is a separate future task. Top
priority for Phase B: A0 (measure valkey c100) BEFORE any code change.
Without A0, A6 / A9 / A14 polish would land but no one could tell if
the result moved the needle vs the real ceiling — repeating the
2026-06-20 anti-pattern.*
