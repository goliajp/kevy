# stream_groups_survive_bgrewriteaof_restart — ROOT-CAUSED & FIXED

- Occurrences: CI run 31546527906 (first, on a docs-only commit under
  uring), then 8/10 locally under full-suite parallelism once S3 gave
  kqueue the same append lag. Fixed same day on the S3 follow-up
  branch.
- **Root cause — a test-harness assumption, not a PEL bug**: the
  test's swap discriminator ("aof-0.aof no longer contains
  XREADGROUP") assumed appends hit the disk synchronously. Under the
  AOF offload (uring since R2b; kqueue/epoll since S3) the on-disk
  file LAGS the replies, so an early read of the not-yet-written log
  matched spuriously, the test stopped the runtime before the rewrite
  swapped, and the restart replayed the RAW history — whose XPENDING
  is legitimately 3/2/1 against the rewrite-semantics expectation of
  2/1/1. (This note's first draft flagged "total ≠ Σconsumers
  inconsistency"; that was a byte-misread of the assert dump — the
  actual reply was a self-consistent 3/2/1. Verified by replaying two
  captured failing AOFs and a synthetic raw history: all give 3/2/1,
  identical to live execution. No replay divergence exists.)
- Fix: the discriminator now waits for the XCLAIM frames only the
  rewritten image contains. 8/10 → 0/10 under the same load.
