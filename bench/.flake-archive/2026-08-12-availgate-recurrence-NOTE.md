# availgate crash-failover convergence timeout — SECOND occurrence

- Run: 31542402364 (branch feature/s2-crashgate-server-always,
  CI ubuntu). Full job log archived alongside this note.
- Change surface: S2 always-CQE-gate — unreachable here (availgate is
  entirely `--no-aof`; every S2 branch gates on an AOF with
  appendfsync=always).
- Shape (same as the 2026-08-10 first occurrence): crash failover
  itself PASSES ("7460 won and opened writes"), then the surviving
  replica misses the convergence window. This log adds detail the
  first one lacked: BOTH nodes transiently log "this node is PRIMARY
  (writes open)" with feed generation bumped to 2 before one demotes
  to "following new primary 'n2'" — a dual-primary election transient
  on a slow runner, resolved correctly but after the gate's assert
  window.
- Disposition: rerun was green. Per the ledger rule ("再犯追根因")
  this is now PAST the flake allowance — the root-cause hunt is a
  booked arc: reproduce with an artificially slowed elect loop
  (timer skew / sched jitter), and either widen the convergence
  window to cover the demote-then-follow transient or close the
  dual-primary window in kevy-elect itself.
