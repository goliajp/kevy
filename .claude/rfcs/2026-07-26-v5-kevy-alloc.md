# kevy-alloc — a per-shard allocator, because the fragmentation is not tunable

> v5 **experiment** arc, first train. **Status: DESIGN — not approved, no code.**
>
> **Header drift, noted 2026-08-06.** The line above says "not approved,
> no code". Both halves are contradicted by the record: `.claude/ROADMAP.md`
> marks this RFC approved, and a train was built against it on the
> `r1-locality` branch (the allocator reached a v8 closing ledger there).
> This note states what is verifiable rather than rewriting the status —
> whether "approved" is the owner's word is theirs to say. What must not
> stand is a header that sends the next reader looking for permission
> that the work already assumed.
>
> **This is an attempt at v5, not v5** (owner, 2026-07-26). Everything below is
> a hypothesis under test, revisable down to its premises. If measurement kills
> a premise, the premise changes — the design is not patched around it, and the
> negative result is written up as a result. Being listed here does not make
> any of it settled, and §4 names references without promising to reproduce
> them: where our model points somewhere else, we go there and say why.
>
> Design input: `.claude/plans/2026-07-26-v5-arc-design-input.md`.
>
> **Framing (owner, 2026-07-26):** *this is not an improvement, it is a design*
> — a new model coming out of the lab and being turned into a product. So the
> target is not "some percentage better than glibc"; it is **the ceiling the
> model actually permits**, with the structure that makes that ceiling
> reachable. Every criterion below is written that way.
>
> Constraint ruling, same day: zero-dep stays a hard rule — *"build it
> ourselves, learn from the good open source projects, and only do it better."*
> So the allocator is ours to write, not to depend on.

## 1. The measured problem

Two runs on real hardware, same cause, different value sizes:

| run | values | logical (`used_memory`) | RSS | ratio |
|---|---|---:|---:|---:|
| PG comparison, 12 GB dataset | ~400 B | 1.96 GB | **4.39 GB** | **2.24×** |
| capacity envelope B6, 5M SET | 4 KiB | 1.586 GB | 2.61 GB | 1.65× |

Tiering's logical bound is honoured in both. The excess is resident pages the
OS never gets back, and **smaller values make it worse** — more chunks, more
interspersion.

The mechanism, from
[`PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md`](../../bench/PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md):
glibc's `M_MMAP_THRESHOLD` is 128 KB, so ordinary values come from the **brk
arena**, which can only shrink from the top. Once demotion frees a chunk that
sits below a live one, that page is stuck.

Two standard levers were measured and **do not help**:

- `malloc_trim(0)` returns 1 ("did work") and RSS does not move — a standalone
  repro (500k × 4 KiB, free half, trim) held at 1974 MB before and after.
- `MALLOC_ARENA_MAX=2` gives byte-identical RSS (2611 MB).

**This is not reachable by tuning.** For a company whose budget line is RAM,
it is also the single most expensive number we have.

## 2. Zero-dep held — and it is not a handicap here

The finding listed "relax 0-dep, use jemalloc/mimalloc" as one of three
options. That option is **rejected** by the ruling above. What remains is not a
consolation prize:

**A general-purpose allocator must serve C's `free(ptr)`, which carries no
size.** That forces a per-chunk header on every allocation — glibc pays 8–16
bytes and, worse, interleaves those headers through the heap, which is part of
why brk cannot shrink. torajs hit exactly this: its libc-compat shim needed a
16-byte `SHIM_HEADER` purely to re-derive the size that `free` did not pass.

**kevy has no C callers.** Rust's `GlobalAlloc::dealloc` receives the `Layout`.
An allocator that only ever serves sized deallocation needs **no header at
all** — the size class comes from the caller, and the span registry maps
pointer → span for anything else. On ~400 B values that is a few percent of
footprint given away for free, and structurally it removes the interspersed
metadata that makes reclaim impossible.

That is the concrete content of "only do it better": we are not writing a
worse mimalloc, we are writing an allocator for a narrower contract than
mimalloc is allowed to assume.

## 3. Prior art we already own

### 3.1 `torajs-mmalloc` — the architecture, already load-bearing

`~/workspace/goliajp/torajs/crates/torajs-mmalloc`, 2867 LOC across 11 modules,
one dependency (`torajs-syscall`), **used by 14 torajs crates** and wired as
that project's `#[global_allocator]`. Its stated references are mimalloc's
`page.c`, tcmalloc's `ThreadCache`, Go's `mheap.go`, and snmalloc's slab — the
same reading list this RFC would otherwise start from.

What it already contains:

| piece | shape |
|---|---|
| `span.rs` | 16 KiB mmap'd span, one size class per span, inline LIFO freelist in unused slots |
| `size_class.rs` | 9 power-of-two buckets, 16 B … 4096 B |
| `large.rs` | `> 4096 B` → direct mmap, munmap on free |
| `span_registry.rs` | pointer → span lookup |
| `tlab.rs` | per-thread cache, depth 16 per class, ~1.2 KB static, non-atomic push/pop |
| `central.rs` | Treiber MPMC stack for **foreign-thread frees**, drained into the owning TLAB on alloc-miss |
| `global_alloc.rs` | `GlobalAlloc` impl incl. the over-align pre-header path |

**Everything is mmap/munmap-backed — no brk.** That is the reclaim property
the measurement in §1 says we need, and it is the reason this architecture is
the answer rather than a value-buffer pool bolted onto glibc.

### 3.2 Two pieces of tuition torajs already paid

**The cutover regressed alloc-heavy paths before the thread cache existed.**
`docs/v0.7-A2-finding.md` records the first cutover costing ~10–30 ns per
alloc against libc's nano-allocator thread cache — `generic-pair-1m` went from
1.6× faster than Rust to 2.4× slower (a 4× reversal), `array-sum-1m` 1.8×. The
fix was the TLAB. **For kevy this is not a footnote — it is the whole risk**
(§6), and it means the thread cache is not a later optimisation but part of
the first shippable shape.

**Unbounded span pools are a crash, not a leak.** Commit `c2970b6d` added
`PER_CLASS_CAP = 4096` spans (64 MB/class) after a legal program allocating
300k live WeakMap keys exhausted a class, got `None` from the allocator, and
propagated a null into a write → SIGSEGV. A cap plus an honest OOM path is
required from the start.

### 3.3 `kevy-madvise` — the OS boundary already exists

`crates/kevy-madvise/src/lib.rs` already hand-binds `mmap` / `munmap` /
`madvise` with `unsafe extern "C"` (no `libc` crate) and exposes
`mmap_anon_aligned_2mb` / `munmap_2mb` / `advise_hugepage`. kevy-alloc needs no
new OS boundary — it consumes this one, which is also where a huge-page-backed
span would come from.

### 3.4 `spg_crypto::lzss` — noted, not this RFC

`~/workspace/goliajp/spg/crates/spg-crypto/src/lzss.rs`, 356 LOC, no_std, zero
deps, in production for spg's WAL and segment compression with an e2e gate
asserting ≥20 % smaller. ~2× on text, ~50 MiB/s encode, ~100 MiB/s decode.
A real starting point for `kevy-compress` — and a clear target to beat, since
LZ4-class decode is an order of magnitude above that, and cold-read latency is
decode-bound. **Deferred to its own RFC.**

## 4. What our model gives us that mimalloc cannot assume

This is where "better" has to come from, so it should be stated precisely.

- **I1 share-nothing ⇒ the thread cache is the heap.** tcmalloc and mimalloc
  keep a thread cache in front of a shared central heap because they cannot
  know how threads relate. kevy pins one shard per core and routes every key
  to its owner: a shard can own its spans outright, so the common path is not
  "cache hit avoids a lock" but **"there is no lock to avoid."**
- **Sized dealloc everywhere** (§2) — no headers.
- **The value size distribution is observable**, per shard, at runtime. Size
  classes need not be static powers of two; they can be fitted. (Design it,
  measure it, then decide — not a v1 promise.)
- **Demotion is a known event.** When tiering demotes, the engine knows the
  buffer is dead *and* its exact size *and* that a batch of them is coming.
  That is a batched, header-free, size-known free — the exact operation glibc
  handles worst and the one that produced §1's number.

## 5. Design sketch

A stone crate `kevy-alloc`, which necessarily carries `unsafe`.

> **Corrected at T0.** This section first said unsafe lived in
> "`kevy-sys` / `kevy-uring` / `kevy-madvise` — a few crates". Recording the
> set for allocgate's M8 showed **fourteen** crates actually contain
> `unsafe {`/`fn`/`impl`/`extern`/`trait` outside tests: the three above plus
> `kevy-bytes`, `kevy-chaos`, `kevy-ffi`, `kevy-jni`, `kevy-lua-host`,
> `kevy-map`, `kevy-napi`, `kevy-ring`, `kevy-rt`, `kevy-vector`, `kevy-wasm`.
> That is what an engine with FFI doors, a wasm ABI, a raw-entry map and a
> uring reactor looks like, and the claim was simply wrong.
>
> What is worth gating is therefore not a small number but that the number does
> not quietly grow. M8 became a **ratchet** on the recorded set
> (`bench/.unsafe-crates-baseline`), with `kevy-alloc` pre-approved as a
> deliberate addition. It runs today and is the one line green at T0.

```
alloc(layout):
  class = size_class(layout.size())         // small path
    → shard TLAB pop                        // no atomics
    → on miss: drain foreign-free queue into TLAB
    → on miss: carve from this shard's partial span
    → on miss: new span via kevy-madvise mmap  (respect PER_CLASS_CAP)
  large (> class max) → direct mmap, page-aligned

dealloc(ptr, layout):
  owning shard?  → TLAB push (cap → spill to span freelist)
  foreign shard? → Treiber push onto the owner's foreign-free queue
  span fully free + pool above low-water → munmap   ← the reclaim property
  large → munmap
```

Two properties are load-bearing and both must be gated, not assumed:

- **Reclaim**: an empty span is returned to the OS. This is what §1 needs; it
  needs a hysteresis policy so a churny workload does not mmap/munmap-thrash.
- **Foreign frees are real in kevy.** Values are `Arc<Box<[u8]>>` and the
  shared read lane hands them across shards, so an `Arc` can drop on a
  non-owning shard. torajs shipped `central.rs` scaffolded but never
  integrated (single-threaded runtime); **kevy must integrate it on day one**,
  and its documented-and-accepted ABA hazard has to be re-examined rather than
  inherited, because kevy genuinely runs N cores freeing concurrently.

## 6. The regression risk *is* the project

The owner's fourth ruling: *"KV usage does not change and must not regress;
pubsub likewise."* Combined with torajs's measured 4× reversal on alloc-heavy
paths, this is the design's binding constraint, not a closing caveat.

One asymmetry makes it sharper than it was for torajs: **an allocator has no
off switch.** The capacity arc could gate itself with "tiering compiled in but
OFF ⇒ byte-identical hot path" (its A1 criterion). kevy-alloc cannot. Whatever
it costs, it costs on every `SET`, every `GET`, every published message.

So the acceptance criterion is not "no regression when disabled" but **no
regression when enabled**, on the existing perfgate lines, at the existing
tolerance.

## 7. Adoption — and why the "narrower" option is actually the invasive one

The B6 finding proposed a value-buffer pool as the 0-dep-safe, narrow option.
**On stable Rust it is the more invasive of the two.** Choosing an allocator
per allocation requires `allocator_api` (unstable); doing it without that means
replacing `Arc<Box<[u8]>>` with a custom smart pointer across the value path —
which is precisely the 5-crate refactor v1.29 already did once, for a
throughput-neutral result.

`#[global_allocator]` is stable, needs **zero call-site changes**, and covers
the index and reactor allocations too — which the value pool would not.

Staging, therefore:

1. **Stone first.** `kevy-alloc` standalone: unit tests, fuzz (the churn
   shape that produced §1), and a bench harness comparing against the system
   allocator on synthetic distributions. Nothing wired.
2. **Wire behind a feature, default off.** The `kevy` binary sets
   `#[global_allocator]`; perfgate runs both ways. This is where §6's gate
   either passes or the design goes back.
3. **Default on** only once the KV and pubsub lines hold with it enabled, and
   B6/B8 RSS is measured on the same workload that produced §1.

Note for embedders: a library must not impose a global allocator, so
`kevy-embedded` users opt in by setting `kevy_alloc::KevyAlloc` themselves. The
server binary decides for itself.

## 8. Acceptance criteria (gate-carried; no criterion without an assertion)

| # | criterion | carrier |
|---|---|---|
| **M1** | perfgate KV lines (GET/SET/pipeline) within existing tolerance **with the allocator enabled** | perfgate |
| **M2** | pubsub lines likewise | perfgate / pubsub bench |
| **M3** | RSS − `used_memory` is fully accounted for by the four terms in §8.1, each measured separately, and **only the rounding term scales with the dataset** | capacity-envelope B6 + a new 400 B variant + an allocator-internal accounting export |
| **M4** | reclaim proven directly: allocate N spans, free them, assert RSS returns | kevy-alloc integration test |
| **M5** | foreign-shard free correctness under N-core churn | fuzz + a multi-shard stress test |
| **M6** | per-class cap honoured; exhaustion is an honest OOM, never a null deref | unit test (torajs `c2970b6d`'s lesson) |
| **M7** | every existing gate green (crashgate/availgate/tiergate/tablegate/textgate/oracle) | existing |
| **M8** | `unsafe` appears in no crate outside the T0-recorded set (`kevy-alloc` pre-approved) — a ratchet, not a count | allocgate (**green at T0**) |

M3 is the reason the arc exists; **M1 and M2 are the reason it could be
rejected.** Both must be measured on lx64, not on a laptop.

### 8.1 What the ceiling actually is

"Materially better than 2.24×" is an improvement target, and this is not an
improvement project. The right statement is structural: after this design,
`RSS − used_memory` consists of exactly four terms, and three of them do not
grow with the dataset.

| term | what it is | scaling |
|---|---|---|
| **rounding** | a 400 B value in a 512 B class wastes 112 B | **O(live bytes)** — the only scaling term |
| **span slack** | pages in a partially-filled span whose slots are not handed out | O(classes × shards) — bounded, ~single-digit MiB |
| **cache retention** | free slots held in a shard's TLAB | O(classes × shards × depth) — bounded |
| **hysteresis** | empty spans deliberately kept to avoid mmap/munmap thrash | O(low-water policy) — an explicit, bounded knob |

Everything glibc adds beyond these — per-chunk headers, brk pages that cannot
return, interspersed metadata — is **eliminated by construction**, not reduced.
That is the design claim, and it is what makes the number fall out rather than
being chased.

So the ceiling is set by size-class rounding alone. Power-of-two classes waste
~25 % on average; tcmalloc-style graded classes hold worst-case waste near
12.5 %. And because §4 lets a shard **observe its own value size
distribution**, the class table is fittable rather than fixed — which is where
"better than a general-purpose allocator" stops being rhetoric.

**M3 therefore asserts the decomposition, not a ratio.** The allocator exports
all four terms; the gate requires that they sum to the observed gap (so nothing
is unexplained) and that only the first grows with data. A headline ratio falls
out of that and goes in the docs — it is a consequence, not the target.

## 9. Decisions (owner, 2026-07-26)

**Rewrite, standing on the shoulders — not port.** torajs-mmalloc is
single-threaded by construction (`static mut` globals, `central.rs`
scaffolded-not-integrated) and aimed at a no-libc metal binary; kevy needs
per-shard heaps and genuinely concurrent frees. What transfers is the design
judgement, and §3.2's two lessons are worth more than the LOC. The reference
list is explicit, and each entry names what it is here for:

| source | what we take |
|---|---|
| **mimalloc** (Leijen et al.) | free-list sharding per page; the local/thread-free split that keeps the fast path atomic-free |
| **tcmalloc** | graded size classes bounding worst-case rounding near 12.5 % — the term that sets our ceiling (§8.1) |
| **Go runtime** `mheap`/`mcache` | span-per-size-class ownership, and heap accounting as a first-class exported thing |
| **snmalloc** | message-passing for cross-thread frees — the closest published fit to share-nothing shards |
| **jemalloc** | decay-based page return; the hysteresis policy that keeps reclaim from thrashing |
| **torajs-mmalloc** | a working mmap-backed realisation of the above, plus §3.2's two paid-for lessons |

**Huge pages: a hint, not a structure.** Ceiling-first cuts both ways here —
2 MB pages cut TLB misses, but a partially-used huge page is *fully resident*,
which inflates the exact metric §8.1 gates. Architectural clarity resolves it:
**the span is the unit of ownership and of reclaim, and it must stay
fine-grained.** `MADV_HUGEPAGE` is advisory (the kernel splits on partial
unmap), so it can be applied per region without changing that structure. It is
therefore a measured knob evaluated against M1 and M3 — **not** a design
commitment, and explicitly not `MAP_HUGETLB`.

## 10. Not in this RFC

`kevy-compress` (its own RFC; §3.4 has the starting point) · the
auto-declaration loop · index-layer hot/cold windows · index-as-key ·
the reverse-proxy deployment recipe for the auth question.
