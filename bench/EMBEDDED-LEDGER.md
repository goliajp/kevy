# EMBEDDED-LEDGER — kevy embedded scalar path vs per-language native stores

The server-language companion to `bench/mmkvgate/LEDGER.md` (the mobile
track, kevy vs MMKV, real-device measured). This ledger records kevy's
embedded scalar `get`/`set` head-to-head against each language's native
embedded store, **losing axes named, not hidden** — the north star
(roadmap t4) is to beat the native store on every axis; the honest starting
point is that no such comparison had been run.

Design + competitor selection + fairness framework:
`.claude/rfcs/2026-07-23-v4-embedded-bench.md`. Read it first — it defines
the durability tiers, the sync-vs-async split, the cold-single-op vs
amortized axes, and why LiteDB is a document-store reference rather than the
C# KV peer (LMDB via Lightning.NET is).

## Rules (from the RFC — do not violate in a table)

- **Compare within a durability tier only.** T-mem / T-async / T-fsync
  never cross in a verdict column. Each harness prints both sides' exact
  durability config so the tier match is auditable.
- **Sync and async never share a table.** classic-level (async) is a
  labeled reference block, not a latency peer.
- **Report cold-single-op AND amortized.** kevy's scalar number is the same
  in both (no txn to amortize); the competitors' differ (txn/stmt setup).
- **Relative standing from dev-host runs; absolute SLA from lx64.** Per perf
  methodology §9 — the mmkvgate SET refutation (sim inflated the write path,
  real ext4 flipped it) is the precedent.

## Competitor versions (pinned as measured)

| Lang | Peer | Version | Model | Sourced |
|------|------|---------|-------|---------|
| Go | bbolt (`go.etcd.io/bbolt`) | v1.5.0 (2026-06-03) | mmap B+tree, 1 writer | pkg.go.dev |
| Go | badger (`github.com/dgraph-io/badger/v4`) | v4.9.4 (2026-07-08) | LSM+vlog, SSI | pkg.go.dev |
| Node | better-sqlite3 | v13.0.1 (2026-07-21) | SQLite, **sync** | npm |
| Node | classic-level | v3.0.0 (2025-04-20) | LevelDB, **async** | npm |
| C# | LightningDB (LMDB) | 0.22.0 (2026-07-05) / LMDB 0.9.33 | mmap COW B+tree | nuget |
| C | LMDB | 0.9.33 (2024-05-21) | mmap COW B+tree | openldap |

---

## Results

_Pending — harnesses under `bench/embeddedgate/<lang>/`. Each track's table
lands here as it is measured (dev-host relative standing first, lx64
definitive pass second). Losing axes named per the rules above._

### Node — kevy-node vs better-sqlite3 (sync) / classic-level (async)

_pending_

### Go — kevy-go vs bbolt / badger

_pending_

### C — kevy C ABI vs LMDB

_pending_

### C# — kevy C# scalar vs LMDB (Lightning.NET)

_pending_
