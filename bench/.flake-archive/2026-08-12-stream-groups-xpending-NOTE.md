# stream_groups_survive_bgrewriteaof_restart — XPENDING count flake (first occurrence)

- Run: 31546527906 (feature/s2-crashgate-server-always, DOCS-ONLY
  commit 3c5d73b0: .gitignore + a flake note). The same job PASSED on
  48d06ae7 — the branch's full S2 engine change — so the diff between
  green and red cannot reach the engine: probabilistic, pre-existing.
- Shape (from the archived log): after XADD ×3? / group setup /
  BGREWRITEAOF / restart, XPENDING summary reports total **:3** while
  the per-consumer breakdown says c1=1 + c2=1 — the reply is
  INTERNALLY inconsistent (total ≠ sum of consumers), which points at
  a real PEL accounting seam somewhere in the rewrite-window replay
  (double-counted pending entry?), not at test-harness timing.
- Disposition: rerun. First occurrence in the ledger — but unlike a
  pure timeout flake this one carries an inconsistent-state signature,
  so on recurrence the hunt should start at stream PEL
  serialize/replay under a rewrite tee window, not at the harness.
