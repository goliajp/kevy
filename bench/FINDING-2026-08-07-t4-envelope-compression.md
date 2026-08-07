# T4 at envelope scale: the corpus claim pays, and the gate goes green

Full-scale capacity-envelope on the T4 tip (compression wired through
the vlog), lx64, disk-backed TMPDIR. Every phase PASS; compressgate's
eight K lines are now all real assertions, all green.

| K line | result |
|---|---|
| K1 cold-read budget | scalar p99 **128 µs** /300 · hash-row **130 µs** /500 — decode is invisible inside the band (pre-compression scalar baseline was ~105 µs) |
| K5 amplification | **0.01×** vs the 2.0× bound — ~19.4 GB of live cold bytes stored in a **273 MB** vlog (~71×); the uncompressed baseline measured 1.27× |
| B6 / B8 | data:RAM 10.1× at 4 KiB values; used_memory ≤ budget × 1.05 throughout |
| sweep / D1 / D4 / L12-L14 | 14/14 op sweep; hydration 402 µs p95, one pread per row, batched; hot p99 trip-wire held |

## The two honest caveats

- The envelope's B6 corpus is ONE random 4 KiB value repeated 5 M
  times — per-datum incompressible, cross-value maximally redundant.
  0.01× is therefore the **ceiling end** of the K4 category claim at
  scale, not a general figure; the measured realistic middle is the K4
  premise table's templated-JSON 2.2× (dict) vs 1.7× (per-datum). The
  general statement stays qualitative: cross-value redundancy that a
  per-datum compressor cannot see, captured at file scope.
- With amplification this low the churn phase never crossed the
  compaction threshold (epoch stayed 0), so "compact_below still
  terminates" was exercised only at unit scale this run. It has its
  own tests; noting the gap rather than claiming the line covered it.

## What flipped and where it is pinned

- K1/K5-amp consume `bench/.capacity-envelope-results` (the tiergate
  pattern; full-scale runs only).
- K7 rides the structural fact (the dictionary is a `VlogFile` field,
  never serialized) plus the vlog disposability contract test — the
  B10/B11 envelope halves belong to tiergate's L10/L11.
- K2/K3/K4/K5-identity run the crate and vlog unit tests directly;
  fuzz targets `roundtrip` / `decode_arbitrary` stand beside them.

---

## Our codec against the oracle table (same corpora, measured not estimated)

`examples/k4_corpora.rs` mirrors `bench/k4_premise.py`'s four corpora
through `kevy_compress` itself (B/value, dictionary bytes counted):

| corpus | per-datum ours/oracle | shared-dict ours/oracle |
|---|---:|---:|
| identical | 98.0 / 89.0 | **9.4 / 41.8** (dict 400 B after dedupe) |
| templated | 354.4 / 231.6 | 264.4 / 180.0 |
| random | 403.0 / 411.0 | 446.8 / 405.7 |
| textual | 231.7 / 148.8 | 240.6 / 103.8 |

Three facts fall out:

1. **train() learned its first measured lesson**: the un-deduped v1
   filled its budget with 164 copies of the identical value — 65 B/val
   of amortized dictionary buying nothing. Exact-duplicate dedupe
   (FNV-identity, collisions harmless) landed and the identical corpus
   now beats the oracle 4.4× — one whole-value token versus zlib's
   per-record deflate overhead.
2. **The residual gap to the oracle is literal entropy, not match
   finding**: dictionary capture works (templated payload 354→199
   before amortization) but our token stream stores literals raw where
   zlib Huffman-codes them — exactly RFC §7.1's named follow-up level,
   now with its price measured per corpus.
3. **On incompressible corpora a dictionary is pure cost** (random:
   shared-dict worse than per-datum by the amortization plus a
   sample-inclusion artifact). At vlog scale this is 64 KiB per 256 MiB
   file (0.02 %) — noted, not actioned.

---

## The compaction level lands — and prices its own limit

RFC §7.1's named follow-up (owner-decided 2026-07-26: "the compaction
level lands afterwards against the same K-criteria") is in: one match
finder, two serializers. The high level pulls all literals into one
canonical-Huffman block (zstd's shape: bulk literal decode, then a
byte-aligned sequence stream), is strictly smallest-wins against fast
and raw (K2 holds by construction one level up), and
`Vlog::compact_step` now rewrites survivors through it — the two-stage
trade riding the scan that already exists.

Measured immediately, in both directions:

- On block-scale text (20 KB JSON rows) the entropy layer beats the
  fast level by >5 % — asserted in a unit test, not observed once.
- **On the 400 B corpora it never engages**: the 128-byte
  code-lengths header does not amortize inside a small value's literal
  block, huff::encode K2-refuses, and smallest-wins falls back to the
  fast frame. The corpora table's dict+high column equals the dict
  column at 400 B — by design, and honestly.

The structural lesson goes on the follow-up list rather than into the
format tonight: at kevy's small-value scale the entropy table wants to
be **file-scoped, riding the dictionary lifecycle** (zstd embeds
entropy tables in its dictionaries for exactly this reason) — one
header per file instead of per record. Format churn deferred; the
level's plumbing (tags, smallest-wins, compaction wiring) is where a
shared-table refinement drops in without touching callers.
