# The v5 accounting contract

> Fixed at T0, before either crate exists. Both RFCs state their ceiling as a
> **decomposition** rather than as a target ratio, and a gate that cannot
> assert *"these terms sum to the observed gap"* cannot check the only claim
> that matters. So the exports come first and the implementations are built
> against them.
>
> Owners: `bench/allocgate.sh` (M3) · `bench/compressgate.sh` (K5) ·
> RFCs `.claude/rfcs/2026-07-26-v5-kevy-{alloc,compress}.md`.
>
> **This is an experiment.** If a term turns out to be the wrong cut of
> reality, change the contract and say why — do not add a fudge term to make
> the identity close.

## Why an identity and not a budget

A budget ("RSS must be under X") tells you that you failed, not what failed. An
identity tells you which term moved. Every criterion below is written so that
an unexplained byte is a **gate failure**, not a rounding note — because the
unexplained bytes are exactly where glibc's 2.24× was hiding.

---

## 1. Allocator (`kevy-alloc`)

Exported per process, summed across shards. Names are the wire names; the
transport is INFO (a `# Allocator` section, following the capacity arc's
`# Tiering` precedent) plus a direct accessor for the embedded API.

| field | definition |
|---|---|
| `alloc_mapped_bytes` | total bytes currently mapped from the OS. **The anchor** |
| `alloc_live_bytes` | Σ `Layout::size()` over live allocations — what callers actually asked for, unrounded |
| `alloc_rounding_bytes` | Σ (`class_size` − `Layout::size()`) over live allocations |
| `alloc_span_slack_bytes` | mapped bytes in partial spans that are neither live nor on a free list (never yet handed out) |
| `alloc_cache_bytes` | bytes held in per-shard TLABs and foreign-free queues |
| `alloc_hysteresis_bytes` | bytes in fully-empty spans deliberately retained by the return policy |
| `alloc_large_bytes` | bytes in direct-mmap allocations above the largest size class (page-rounded; its rounding lands in `alloc_rounding_bytes`) |

### The identity M3 asserts

```
alloc_mapped_bytes
  == alloc_live_bytes
   + alloc_rounding_bytes
   + alloc_span_slack_bytes
   + alloc_cache_bytes
   + alloc_hysteresis_bytes
```

Exact, not approximate: every mapped byte is in exactly one of those states by
construction. A tolerance here would be a place for a leak to hide.

### The scaling claim M3 asserts

Across the B6 workload at two dataset sizes, **only `alloc_rounding_bytes` may
grow with the data.** `span_slack` and `cache` are O(classes × shards);
`hysteresis` is O(the return policy's low-water mark). If one of the three
grows with the dataset, the design is wrong — that is the finding, and it is
worth more than a passing gate.

### Relating to RSS

`alloc_mapped_bytes` is what the allocator controls. RSS additionally contains
the binary, thread stacks, page tables, and anything still allocated through
paths kevy-alloc does not serve. The gate therefore reports
`RSS − alloc_mapped_bytes` as a separate, named residual and requires it to be
**flat in the dataset size** — a residual that grows is an allocation path we
failed to notice, which is a finding rather than a tolerance.

---

## 2. Codec (`kevy-compress`)

Exported per vlog file, and summed across files for INFO.

| field | definition |
|---|---|
| `cmp_stored_bytes` | bytes the file actually occupies for values. **The anchor** |
| `cmp_raw_bytes` | Σ original value lengths (what it would have cost uncompressed) |
| `cmp_dictionary_bytes` | the dictionary stored in this file |
| `cmp_frame_overhead_bytes` | Σ per-record frame header bytes (tag + original length) |
| `cmp_payload_bytes` | Σ encoded payload bytes, compressed and raw frames alike |
| `cmp_incompressible_bytes` | Σ payload bytes of frames stored raw (the never-expand path took over) |

### The identity K5 asserts

```
cmp_stored_bytes == cmp_dictionary_bytes + cmp_frame_overhead_bytes + cmp_payload_bytes
```

### The honest limit, stated rather than papered over

The compress RFC §6.1 names four terms, and two of them —
*incompressible residual* and *match-finder misses* — **are not separable by
accounting alone.** Knowing how much redundancy the encoder failed to find
requires knowing how much was there, which is the compression problem itself.

So the contract does not pretend. `cmp_payload_bytes` is the sum of both, and
the split is measured **out of band** against an oracle rather than asserted in
the gate:

> `spg_crypto::lzss`'s `find_longest_match` walks every window position — it is
> a **brute-force exhaustive matcher**. Run it over a sample corpus, offline,
> and the difference between what it finds and what our hash matcher finds *is*
> the match-finder miss term, measured rather than estimated.

That comparison belongs in a finding doc when T3 lands, not in a per-run gate.
Recording the limit here is the point: a term we cannot separate is stated as
one term, not split with a guess.

---

## 3. What both gates refuse

- **An unexplained byte.** Identity mismatch is FAIL, never a warning.
- **A tolerance added to make an identity close.** The identities above are
  exact by construction; if one does not close, something is miscounted and the
  gate has done its job.
- **A term invented after the fact to absorb a discrepancy.** Changing the
  contract is allowed — silently widening it is not. A changed contract comes
  with the reason, in this file, in the same commit as the change.
