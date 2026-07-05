# v3.9-t1 onramp drill — mailrs-shaped rehearsal, findings

Harness: bench/drill_mailrs.sh (290k keys / 703MB across five
prefixes: 200k multi-KB hash bodies, 20k zset mailboxes, 20k user
hashes, TTL'd sessions). ALL seven steps PASS on lx64.

## Timings (the migration road, measured)

| step | result |
|---|---|
| export 290k keys / 703MB | 12s |
| import --strict (630k commands) | 2s ≈ 315k cmd/s (onrampgate line: ≥200k) |
| per-prefix digest, 5 prefixes both ends | all equal |
| kill -9 → --resume | converges; no-op resume correct |
| post-load text+range backfill (200k big bodies) | 17s |
| copy-prefix 20k @ --rate 5000 | 4.0s (exact) |

## UX gaps found → disposition

1. **No-op resume read as silence** — `imported: 0 ok, 0 errors` on
   an already-complete file looks like failure. FIXED: kevy-cli now
   prints "already complete (offset N), nothing to resume".
2. **Index-readiness wait had no documented idiom** — the drill
   poked queries and pattern-matched errors. FIXED in docs:
   migration.md now documents the `IDX.LIST state` polling idiom.
   (v3.10's machine-readable surface will make this first-class.)
3. **diff underused** — one `kevy-cli diff A B p1 p2 …` call replaces
   N digest pairs. FIXED: documented as the standard verification.
4. **Backfill-time scaling with doc size undocumented** — 7s/M small
   rows vs ~85s/M mail-sized bodies (measured). FIXED in docs.
5. **Large exports uncompressed** — fine at 1GB; documented the gzip
   pipe idiom (+ the --resume needs-a-real-file caveat). No new
   flag needed.

No contract-level gaps: every tool did what it promised on a
realistic shape. The road to mailrs is paved.
