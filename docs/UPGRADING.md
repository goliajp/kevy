# Upgrading kevy 2.x → 3.x

kevy 3.x is a superset of 2.x: every 2.x workload runs unchanged, and
the upgrade is a binary swap for the server and a dependency bump for
embedded users. This guide is explicit about what carries over
automatically, what changed names or numbers, and the one direction
that needs care (downgrading back to 2.x).

## TL;DR — versions at a glance

| Component | 2.x era | 3.x | Action |
|---|---|---|---|
| `kevy` (server) | 2.0.x | 3.8.0 | swap the binary, restart on the same data dir |
| `kevy-embedded` | **1.x** (1.4–1.16) | **3.8.0** | bump the dep — the 1.x line ended at v3.0.0 when the whole workspace unified on one version |
| `kevy-client` | 1.12.x | 1.13.x | bump; API unchanged |
| `kevy-client-async` | 1.0.x | 1.1.x | bump; API unchanged |
| `kevy-cli` | unpublished | 3.8.0 | `cargo install kevy-cli` — now carries the whole migration toolchain |
| Infra crates (`kevy-store`, `kevy-rt`, …) | 2.0.x | 3.8.0 | follow the workspace version |

The `kevy-embedded` jump from 1.x to 3.x is a **version-line
unification, not an API rewrite**: the 1.16 surface is contained in
3.x. If your `Cargo.toml` says `kevy-embedded = "1"`, change it to
`"3"` and rebuild.

## What is compatible automatically

**Wire protocol.** RESP is unchanged. 3.x remains reply-checked
byte-for-byte against valkey 9.1 in CI (98 commands). Existing Redis
clients, scripts, and `redis-cli` sessions work as before.

**Snapshots.** The 3.x loader reads every 2.x snapshot format
(`KEVYSNAP` versions 2–5): relative-TTL v2 files, absolute-TTL v3,
stream-group v4, and feed-cursor v5. Point a 3.x server at a 2.x data
directory and it loads.

**AOF.** The AOF is a verb log and 3.x's verb set is a superset of
2.x's — replay works unchanged. `appendfsync` semantics are
unchanged.

**Config.** Every 2.x config key is accepted. New sections
(`[replication] single_source`, `--accept-shards`, …) are additive
with defaults that reproduce 2.x behavior.

## Upgrade steps

### Server deployment

1. Take a snapshot on the running 2.x server (`SAVE` or your normal
   backup), and keep a copy — see “Downgrading” below for why.
2. Stop 2.x, start the 3.8.0 binary with the same flags and data dir.
3. Verify: `DBSIZE` matches, and if you want cryptographic-grade
   assurance run `kevy-cli digest -p <port> <prefix>` before and
   after — equal digests mean an identical keyspace.

Rolling a replica pair: upgrade the replica first, let it re-sync,
then fail traffic over and upgrade the former primary. (2.x has no
managed failover; this is the usual manual swap.)

### Embedded applications

1. `kevy-embedded = "3"` in `Cargo.toml`.
2. Rebuild. The 1.16 API is present unchanged; new capability
   surfaces (index/view/text/vector/feed/replication) are additive
   methods and `Config` options.
3. One trait note: if (and only if) you wrote a custom
   `impl kevy_rt::Commands` and construct `ResolvedCmd` literals,
   two fields were added during the v2 arc (`block_hint`,
   `wake_idx`). The default `resolve()` fills them; literal
   constructors add the two fields.
4. On-disk data from an embedded 1.x app loads as-is (same snapshot
   formats as the server).

### Clients

`kevy-client 1.13` / `kevy-client-async 1.1` are drop-in: the minor
bump only re-pins internal crates to 3.8.0. Generic Redis client
libraries are unaffected either way.

## What 3.x adds (why you upgrade)

Declared indexes with hydration (`IDX.*`), named views (`VIEW.*`),
write-time aggregates (GROUP BY / distributed top-K), dictionary-free
CJK full-text search with BM25, HNSW vector KNN, CDC feeds with the
recovery-point contract (`FEED.*`), embedded-as-primary replication,
and the migration toolchain (`kevy-cli import/export/--verify/diff/
inspect/digest`). Start at [docs/designing-on-kevy.md](designing-on-kevy.md)
and [docs/cookbook.md](cookbook.md); performance receipts live in
[bench/PERF-LEDGER.md](../bench/PERF-LEDGER.md).

None of these activate implicitly: a 3.x server with a 2.x workload
has an empty catalog, and the index hook on an empty catalog is on
the perfgate ratchet (no regression vs 2.x).

## Downgrading (the one direction that needs care)

A 3.x server **writes** snapshot format v4, or v5 once a CDC feed
cursor exists. A 2.x binary reads at most v4:

- If you never enabled feeds, a 3.x snapshot loads on 2.x.
- If feeds were active (v5), 2.x refuses the file. Downgrade path:
  `kevy-cli export` on 3.x → `kevy-cli import` into a fresh 2.x —
  or restore the pre-upgrade backup from step 1 and accept the gap.

Verbs introduced in 3.x (`IDX.*`, `VIEW.*`, `FEED.*`, …) naturally
don't replay on a 2.x binary — if you used them, the export/import
path is the correct downgrade, not AOF replay.

## Version history in one line each

- **3.0.0** — the serving-engine declaration (indexes, views, FTS,
  ANN, CDC, on-ramp; eleven gated trains).
- **3.8.0** — the perf arc (measured vs valkey 9.1 and RediSearch;
  bare face 1.6–3.3×, ANN 1.64× ahead at recall 1.000, FTS single
  common term 93×; embedded-as-primary replication). No releases
  were cut between 3.0.0 and 3.8.0; 3.8.0 contains trains v3.1–v3.8.
