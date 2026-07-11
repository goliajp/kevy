# T9 L4 finding — the huge pages are already there; the probe stalls are a 5.7pp pie

Verdict: L4 (map hugepage / layout for the spread axis) is REFUSED at
the pre-Phase-B gate. Zero code changed; the knife never landed. Both
of the decomp's premises — "hugepage 没用或没生效" and "map probe is
a ≥10pp attack surface" — were measured and are false.

## Gate instrument

lx64, release-perf binary at 05236c1b (code-identical to HEAD), server
cores 0-7 `--threads 8 --no-aof`, client cores 8-15. Prefill
`-t set -r 1M -n 5M` (DBSIZE 993,134), then continuous spread GET
(`-r 1M -c 50 -P 16`, ~4.4-5.3M rps rolling). Dual-event split per the
gate protocol: 10 s `perf stat` (cycles / instructions /
`cycle_activity.stalls_l3_miss` / `mem_load_retired.l3_miss`) + 12 s
`perf record -e cycles:P -e mem_load_retired.l3_miss:P`.

## Gate numbers

| measure | value |
|---|---:|
| IPC | 0.88 (decomp's 0.87 reproduced) |
| `find_by_borrow` cycles self | 8.01% (decomp's 8.29% reproduced) |
| `Store::live_entry` cycles self | 0.33% |
| **map-probe surface, cycles** | **8.34pp < 10pp gate bar** |
| `find_by_borrow` share of L3-miss loads | 65.87% |
| `live_entry` share of L3-miss loads | 31.74% (together 97.6%) |
| **DRAM-stall pie**: `stalls_l3_miss` / cycles | 21.86G / 373.6G = **5.85%** |
| map-probe attributable DRAM stall | 97.6% x 5.85% = **~5.7pp** |
| L3-miss loads per op | 80.2M/10s over ~4.8M op/s = **~1.7/op** |

The dual-event split answers the skid question the decomp left open:
the L3 misses are not smeared into other symbols — 97.6% of them
retire inside the two map/store symbols themselves, and those symbols
still sum to only 8.34pp of cycles. There is no hidden stall mass to
re-attribute.

## Why each candidate dies

- **Hugepage (the "one-liner")**: already fully engaged.
  `/proc/PID/smaps` during the 1M-key run: AnonHugePages 679,936 kB of
  690,760 kB anon (98.4%), with all eight 20,480 kB shard-table regions
  THP-backed — the kevy-map E13 path (`mmap_anon_aligned_2mb` for
  tables >= 1 MiB, `alloc.rs:38`) plus the box's `THP=always` did this
  long ago. The decomp listed L4 assuming the hint was missing or
  inert; smaps refutes that. Zero headroom.
- **Prefetch depth / bucket layout**: both attack only the DRAM-stall
  portion of the probe, and the whole stall pie is 5.7pp. A PERFECT
  attack (every probe line in L1 for free) recovers at most ~5.7% —
  under the +8% landed bar before any implementation cost. The
  remaining 8.01 - 3.9 ≈ 4.2pp of `find_by_borrow` is hash + SIMD
  group scan + key compare — compute, not memory, and not what L4's
  shapes address.
- The miss structure matches the layout model exactly: per shard the
  metadata array is 256 KiB (L2/L3-resident — its misses don't show),
  and the 72 B `(SmallBytes, Entry)` slot straddles two cache lines —
  key line (misses in `find_by_borrow`, 66%) + entry line (misses in
  `live_entry`, 32%) = the measured ~1.7 misses/op. Out-of-order
  execution already overlaps these two dependent-chain misses down to
  a 5.85% stall share.

## Where the spread tax actually lives

The decomp's own §5.2 table says it: spread costs +65% cycles/op
(4,700 → 7,735) but the map-probe delta explains only ~550 c/op of
that. The bulk is batch-density collapse — `drain_inbound_core_slow`
2.17% → 4.45%, `send_to` materializing, malloc/cfree ~2.1% from pool
misses — i.e. the cross-shard forwarding fabric getting 7x sparser
batches. That is L1's surface (shared-read keyspace), not a map-layout
problem. L4 was never a real lever; it was L1's shadow.

Next lever by the data: the L5 io_uring polish basket (the last
unexamined entry in the T9 table), or accept the T9 lever table as
closed — L1 REFUSED (seqlock gate), L2 REVERTED, L3 REFUSED, L4
REFUSED — and let the v4 arc move off the perf axis.
