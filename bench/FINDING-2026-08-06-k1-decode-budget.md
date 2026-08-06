# K1's decode budget, measured: memcpy-class is 0.18 µs, and the floor is ~1 GB/s

K1 asserts that cold-read decode "must be memcpy-class, not
lzss-class", and the compress RFC quantifies the fear: at spg-lzss's
measured 100 MiB/s, a 4 KiB value costs ~40 µs against a 105 µs cold
read p99 budget — 38 % of the budget gone to decode. This measures the
other side: what memcpy-class actually is on the target box, and what a
word-wise decode loop costs beside it.

Instrument: `bench/k1_budget.rs` (research probe, `rustc -O`, pinned to
one core on lx64; the source pool is larger than L2 so copies are not
trivially cache-resident).

| shape | µs / 4 KiB | GB/s | share of the 105 µs budget |
|---|---:|---:|---:|
| memcpy | **0.183** | 22.4 | 0.17 % |
| word-wise LZ decode (wildcopy, mixed 16 B literal / 32 B match tokens) | **0.486** | 8.4 | 0.46 % |
| spg-lzss reference (RFC's measured 100 MiB/s) | ~40 | 0.1 | **38 %** |

A third row — a byte-at-a-time loop — measured 0.08 µs and is
**discarded as a broken instrument**: the compiler vectorised a trivial
fill; it does not represent a real branchy byte-decoder. The honest
slow-class reference stays the RFC's measured lzss figure.

## What this settles for T3

- **"Memcpy-class" now has a number**: 0.18 µs/4 KiB. A wildcopy
  decoder at ~8 GB/s costs 2.7× memcpy and still under **half a
  percent** of the cold-read budget.
- **The floor is ~1 GB/s**: at 1 GB/s a 4 KiB decode is 4 µs ≈ 3.8 % of
  budget — comfortably inside; at 100 MiB/s it is 38 % — out. The
  requirement is therefore not "as fast as memcpy" but **stay above
  ~1 GB/s decode**, which a token-nibble + wildcopy design (the RFC's
  sketch) clears by an order of magnitude even in this naive form.
- Headroom exists for the dictionary lookup too: K4's finding put the
  capture in dictionary construction; this one says the decode side has
  ~20× margin before the budget notices, so the dictionary can afford
  real work at encode time and modest indirection at decode time.

Caveats: single-core, warm-cache probe — a real cold read also pays the
pread and the page fault, which are the budget's existing occupants;
this measures only the decode share being added. And the token mix is
synthetic; the first consumer-shaped corpus should re-run the probe's
decode loop over real frames when T3 exists.
