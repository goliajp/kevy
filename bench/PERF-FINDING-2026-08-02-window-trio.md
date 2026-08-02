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
- The weak one was honest and is now fixed: RUNNING RSS only dropped
  9% at first — freed hash memory was not returned to the OS by glibc.
  A hand-written malloc_trim binding (kevy-sys, linux/gnu only,
  best-effort no-op elsewhere) called at slide frequency closed it:
  **running RSS 838.9 MB (ctrl) vs 446.1 MB (windowed) = -47% with no
  restart** (was 774 MB / -9%). The remaining gap to the 256 MB
  restart figure is segment page cache + index + residual arena —
  kevy-alloc's territory (R1) when it becomes the global allocator.
- `--pipe` reported errors: 1 on both variants identically; DBSIZE and
  probes are exact. Not chased; recorded.
