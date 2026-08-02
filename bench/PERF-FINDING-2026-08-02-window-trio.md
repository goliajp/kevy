# The window's persistence payoff, measured — lx64 trio

Setup: 500k hash rows (id, at=i, ~90B note), single table, shard=2,
`WINDOW at SPAN 100000 BUCKET 10000` (≈80% of rows evict) vs an
identical windowless control. Same fill, same idle window, same
BGREWRITEAOF, same restart. Box: lx64, kevybench, cores 0-7.

| metric | control | windowed | delta |
|---|---:|---:|---:|
| rewritten AOF | 84.6 MB | 18.7 MB | **-78%** |
| restart (to first PING) | 0.40 s | 0.19 s | **-52%** |
| RSS after restart | 646.9 MB | 257.4 MB | **-60%** |
| RSS while running (post-slide) | 851.2 MB | 774.5 MB | -9% |
| row segments on disk | — | 4 files, 68.7 MB | the cold copy |
| DBSIZE / probe | 500000 / ok | 500000 / ok | equal |

Readings:
- The headline three are the T-row-b2 contract, now numbers: cold rows
  leave the rewrite (trailing SEGMENTED frames only), don't replay on
  boot (stub records + stitch), and load as stubs.
- The weak one is honest: RUNNING RSS only drops 9% after the slide —
  freed hash memory is not returned to the OS by the allocator (RSS
  stickiness). That is kevy-alloc / madvise territory (R1), not a
  window defect; the restart number shows the true footprint.
- `--pipe` reported errors: 1 on both variants identically; DBSIZE and
  probes are exact. Not chased; recorded.
