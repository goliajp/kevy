# The v5 accounting contract

> Fixed at T0, before either crate exists. Both RFCs state their ceiling as a
> **decomposition** rather than as a target ratio, and a gate that cannot
> assert *"these terms sum to the observed gap"* cannot check the only claim
> that matters. So the exports come first and the implementations are built
> against them.
>
> Owners: `bench/allocgate.sh` (M3) · `bench/compressgate.sh` (K5) ·
> the kevy-alloc and kevy-compress RFCs.
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

**Landed in 6.4.0** (`INFO allocator`, wire names prefixed `alloc_`; the
direct accessor is `kevy_alloc::thread_stats`, per-thread by nature).
Written here in the v5 arc and unimplemented until then: M3 proved the
identity inside the crate, which is a different claim from an operator
being able to see which term a resident ratio went into. M9 gates the
transport.

Two things the section is careful about, both because an instrument that
cannot fail is worse than none:

* It is **absent** unless a shard reported, so a build on the system
  allocator emits the bytes it emitted before the section existed.
* "Reported" is `mapped > 0`, not "the snapshot existed". The feature
  links the allocator; the `#[global_allocator]` attribute is what makes
  it *the* allocator, and that lives in `main.rs` — so the library, its
  tests, the embedded API and any FFI host can have the feature on and
  allocate somewhere else entirely. `thread_stats` answers `Some` there,
  with nine honest zeroes, and printing `allocator_impl:kevy-alloc`
  over them would name an allocator that is not running.

| field | definition |
|---|---|
| `mapped` | total bytes currently mapped from the OS. **The anchor** |
| `live` | Σ `Layout::size()` over live allocations — what callers actually asked for, unrounded |
| `rounding` | Σ (`slot size` − `Layout::size()`) over live allocations |
| `cache` | bytes parked on foreign-free lists, waiting to be drained home |
| `span_free` | free slots in spans that were handed out before and returned — **touched, therefore resident** |
| `virgin` | mapped and never touched, **not resident**: span bytes at or above the bump cursor, plus whole spans carved with their segment that no class ever claimed |
| `returned` | pages handed back to the OS — mapped, **not resident** (the v2 term): free slots inside a live span, plus whole spans emptied and retired |
| `hysteresis` | retained rather than released and therefore **resident**: empty spans the per-sweep policy keeps for their class, spans whose discard the platform refused, and (T2-v8) parked large mappings in the process-wide retention pool |
| `segment_overhead` | segment headers (one span per segment) |

**Corrected in 6.4.0, and the correction is the point of the section.**
`hysteresis` was every span with no class, which is three opposite
things: never claimed (`virgin`), emptied and given back (`returned`),
and emptied and kept (`hysteresis`). The identity balances whichever
bucket they fall in, so no gate could see it. What an operator saw was
`returned: 0` with 89 % of the map under `hysteresis` — and which of the
three that 89 % really was depended on the host: given back on a 4 KiB
page and reported as held, held on a 16 KiB page and subtracted from the
residency prediction as though given back. One number, wrong in both
directions.

It had two further faces, both of which had been reported as PASS:

* `predicted_resident()` **subtracted** `hysteresis`, so held memory read
  as released. It now subtracts `virgin` and `returned` only.
* M4 — "emptied spans give their pages back", the property the whole
  experiment rests on — asserted `after.hysteresis > idle.hysteresis`
  under a message reading "reclaim returned nothing". That is true on
  both sides of the platform branch, so it passed on a 16 KiB-page
  machine where `discard` is refused and reclaim hands back **nothing**.
  The assertion now names the branch it is in.

### The identity M3 asserts

```
mapped == live + rounding + cache + span_free + returned + virgin
        + hysteresis + segment_overhead
```

Exact, not approximate: every mapped byte is in exactly one of those states by
construction. A tolerance here would be a place for a leak to hide.

### Revised again at T2-v2 — `returned` added

M3 killed the whole-span reclaim rule (a span of the 416 B class returns
nothing until all 157 slots die together; measured yield: 3 %). v2 returns
**pages inside live spans**, which creates a state the v1 partition had no
name for: a free slot whose pages are mapped but no longer resident. That is
`returned`. `predicted_resident` subtracts it. A free slot counts as returned
iff **every** page it overlaps has been discarded — deterministic, so the
identity stays exact; slots straddling a discarded/resident page boundary
count as `span_free` (the conservative side).

### Revised at T1 — two terms added, one removed

The T0 table had five terms and `alloc_span_slack_bytes`. Building the geometry
showed it was not a partition, so it changed. Recorded here with the reason,
which is what this contract asks for; **silently widening** is what is banned,
not changing:

- **`span_slack` split into `span_free` and `virgin`.** Spans hand out slots by
  bumping a cursor, so the region above it is mapped but never touched, and
  therefore not resident. One term would have made address space look like
  memory — the split is the difference between a number that predicts RSS and
  one that does not.
- **`segment_overhead` added.** One span per segment holds the header: 1.6 % of
  every segment, structural and knowable, so it is named rather than folded
  into a neighbour.
- **`cache` no longer includes a thread cache**, because there is none — see
  the RFC's §5 revision. It now covers only foreign-free lists.

### Not a byte count, but exported anyway

`spans_assigned` — spans currently attached to a size class. It is here because
one real defect is **invisible to every byte term**: if allocation claims fresh
spans while emptied ones sit reusable, the identity balances perfectly and the
heap grows anyway. Found at T1 exactly this way.

### The scaling claim M3 asserts

Across the B6 workload at two dataset sizes, **only `rounding` may grow with
the data.** `span_free` and `cache` are O(classes × shards); `virgin` and
`segment_overhead` are O(spans mapped, which is bounded by the class caps);
`hysteresis` is O(the return policy's low-water mark). If one of those grows
with the dataset, the design is wrong — that is the finding, and it is worth
more than a passing gate.

### Relating to RSS

`mapped` is what the allocator controls, and it is *virtual*: `virgin` and
`hysteresis` bytes are mapped without being resident. `Stats::predicted_resident()`
is therefore `mapped − virgin − hysteresis`, and it is named a prediction
because the kernel decides residency, not us.

RSS additionally contains the binary, thread stacks, page tables, and anything
still allocated through paths kevy-alloc does not serve. The gate reports
`RSS − predicted_resident` as a separate, named residual and requires it to be
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
