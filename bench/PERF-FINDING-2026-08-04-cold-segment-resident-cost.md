# Cold segment resident cost — the R2b criterion, formulated

Source-derived (file:line cited), no new mechanism. The criterion
under test (research master plan §R2b): *the cold segment's memory
cost is formulaic, ≪ the hot tree's per-entry cost.*

## What stays resident per OPEN cold segment

A cold segment is a plain file read with `pread`
(`kevy-seg/src/reader.rs:141`, `read_exact_at`) — **data pages are
never resident by design**; they live in the OS page cache at the
kernel's discretion. What the process itself holds per open segment
(`Seg`, `reader.rs:15`) is:

| item | size | source |
|---|---|---|
| file handle | 1 fd | `reader.rs:16` |
| `SegMeta` (records, data_pages, min/max key) | ~64 B + 2 keys | `lib.rs:47` |
| fence table: `(u32, first_key)` **per 4 KiB data page** | data_pages × (28 B + key) | `builder.rs:26,126`; `layout.rs:20` (PAGE = 4096) |

The fence table dominates and is the whole formula:

> **resident bytes per segment ≈ 200 + data_pages × (28 + avg_key_len)**

Per ENTRY, that amortizes to `(28 + key) / entries_per_page`. With
~64 B records (≈60 entries/page) and 32 B keys: **≈1 B of resident
metadata per cold entry**. The hot tree's per-entry cost (BTree node
+ key + value ownership) is two orders of magnitude above that —
the "≪ hot per-entry" criterion holds with room to spare, and the
ratio is bounded by construction: fences/data ≈ (28+key)/4096
< 1.5 % of on-disk size even before page-cache accounting.

## Per WINDOW (not per segment)

- `ColdBloom` — one per windowed path, fixed at construction:
  sized for 4 096 expected cold keys at ~10 bits each
  (`kevy-index/src/segcold.rs:225`, `kevy-window/src/lib.rs:62`) =
  **8 KiB flat**. It does not grow; past ~4 k distinct cold row keys
  the false-positive rate degrades gracefully (each FP costs one
  stray shadow entry, never a wrong answer).
- The open-segment list itself (`Vec<Seg>`) — the in-memory
  directory; the manifest lives on disk.

The text cold family (`TextColdDir`) is a sibling with the same
shape — per-(row,segment) tombstones and stats withdrawal are its
extra resident items — and is left to its own line when R2b closes;
the scalar/composite family above is the capacity-bearing one.

## Criterion verdict

R2b's memory half is formulaic as demanded: a flat 8 KiB bloom per
windowed path plus ~1 B/entry of fence directory, everything else
delegated to the page cache. The scale statement for the R2 total
criterion: cold metadata is a **per-page** (not per-entry) term with
a key-length coefficient — declaring shorter PKs is the only lever a
deployer has, and it is a small one.
