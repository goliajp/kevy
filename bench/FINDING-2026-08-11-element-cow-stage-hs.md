# FINDING 2026-08-11 — element-COW Stage HS: giant hash/set write stalls closed

Branch `feature/element-cow-hs`. Stage L twin:
`FINDING-2026-08-11-element-cow-stage-l.md`. RFC: element-COW under
`.claude/rfcs/` (local). Upstream evidence: `FINDING-2026-08-10-v5-rc-soak.md`
(multi-GB `myhash`/`myset` single keys step the reactor gap into seconds when
written during a rewrite window).

## The claim, measured

Same probe shape as Stage L (box E2E, fill N → `BGREWRITEAOF` → 0.3 s into
the view window, time one write round-trip):

| door | N | first write during view | second write | RSS transient |
|---|---:|---:|---:|---|
| hash (HSET) | 1M | 2.1 ms | 0.18 ms | ~3.6 MB |
| hash | 5M | 1.2 ms | 0.16 ms | ~1.5 MB |
| hash | 20M | **1.4 ms** | 0.12 ms | ~1.5 MB |
| set (SADD) | 1M | 0.1 ms | 0.04 ms | ~1.5 MB |
| set | 5M | 0.3 ms | 0.15 ms | ~1.4 MB |
| set | 20M | **0.3 ms** | 0.17 ms | ~1.5 MB |

Size-independent: the COW cost is one ≤16K-entry bucket (a KevyMap layout
copy, ~1 ms for the hash door's two-SmallBytes slots), not the whole value.
Before this stage the same write paid a whole-`KevyMap` clone — the rc-soak
measured that class of stall at 7-9.5 s on multi-GB single collections.

## What shipped

`Value::SegHash`/`Value::SegSet` — one generic `SegMap<V>` stone
(`crates/kevy-store/src/seg_map.rs`): an extendible-hash directory over
`Arc`-shared buckets. Directory holds bucket indices; buckets live once in a
side vector, so an `Arc<Bucket>` is shared only with snapshot views — the
first cut shared one Arc across an unsplit range's directory slots, and a
write through a non-canonical slot silently forked the bucket (caught by the
100K-key split test; the layout note in the module doc records it). Buckets
split locally; the directory doubles only when the splitting bucket is at
global depth. Promotion at 16K elements from HSET/SADD, the HINCRBY-only
path, and snapshot restore; ≤16K keeps the flat representation
byte-for-byte. Wire formats unchanged. Sharded hashes are not demotable to
the cold tier (they answer Hot); SPOP/SRANDMEMBER keep their
arbitrary-member contract via a length-weighted bucket pick.

## Boundary that remains (per RFC stages)

ZSets (Stage Z — ranktree path-copy) and streams (documented boundary)
still pay whole-value COW.
