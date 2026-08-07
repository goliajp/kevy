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
