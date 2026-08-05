# Migration — the playbook and the toolchain

Two halves. The **playbook**: how to move a live RDS workload onto
kevy in phases, each with its own verification clamp and rollback
point. The **toolchain** (`kevy-cli`): the commands that move data
into, out of, and between kevy servers — and prove the move was
faithful. Modeling questions ("what does my `JOIN` become?") live in
[rds-workloads.md](rds-workloads.md); runnable patterns in the
[cookbook](cookbook.md).

## The phased playbook

Seven phases. The load-bearing property: **at every phase up to write
cutover, the RDS is still the source of truth and rollback is
"repoint the app"** — never "reverse-migrate data". Don't skip
phases; each one's verification clamp is what makes the next one
boring.

### Phase 0 — Inventory

List every table and every query shape the workload actually runs
(your ORM's query log or `pg_stat_statements` is the ground truth,
not the schema). Classify each against the
[rds-workloads matrix](rds-workloads.md): most rows map mechanically;
flag the ones that hit a refused construct (N-way joins in a hot
path, `HAVING`, deep `OFFSET` pagination, ad-hoc reporting SQL).

- **Action:** a written access-path table: query → matrix row → kevy
  shape (index / view / agg / verb).
- **Clamp:** every hot-path query has a mapped shape **before any
  data moves**. Refused constructs get an app-side redesign decision
  now, not at cutover. Ad-hoc analytics stays off kevy — plan the
  warehouse export (cookbook recipe 14) instead.
- **Rollback:** nothing has moved.

### Phase 1 — Schema mapping

Turn the inventory into a declaration script: prefixes, field
layouts, types, then every `IDX.CREATE` / `VIEW.CREATE` line. Apply
the type rules from the matrix now — DECIMAL to integer minor units,
DATETIME to epoch i64, NULL to absent-field — because they are
write-format decisions you cannot cheaply revisit after backfill.

- **Action:** the declaration script, checked into the repo next to
  your DDL history.
- **Clamp:** run it against a throwaway kevy with a few hundred
  representative rows; `IDX.VERIFY` must show `coerce_failures = 0`
  and expected `duplicates`; `VIEW.VERIFY` no order-exclusions;
  paged queries return what the SQL returned.
- **Rollback:** nothing has moved.

### Phase 2 — Change capture (dual-write window opens)

Start capturing RDS writes **before** the backfill snapshot, so
nothing falls in the gap. Two routes:

- **App dual-write** — the app writes to the RDS (still the truth)
  and mirrors each write to kevy. Simple, no new infra; but the
  mirror rides your write path (make it fire-and-forget with a
  failure counter — a lost mirror write is repaired by re-backfill,
  never by blocking the RDS commit).
- **CDC pull** — a binlog/logical-replication consumer (Debezium or
  hand-rolled) translates row events into kevy writes. Decoupled from
  the app, ordered per row, replayable from a binlog position; costs
  connector infrastructure. Prefer it when you can't touch every
  writer, or when writers are many.

Either way the kevy writes must be **idempotent rebuild shapes**
(`HSET` full row, `DEL` + re-add for collections) so overlap with
the backfill is harmless.

- **Clamp:** freshness lag metric on the capture path (event
  timestamp → kevy apply), plus a row-count drift check per prefix
  (`PREFIX.STATS` vs `SELECT COUNT(*)`) that trends to a constant.
- **Rollback:** stop the capture. The RDS never noticed.

### Phase 3 — Backfill

Snapshot the RDS at a known capture position, export to a RESP
command file, and bulk load:

```
# generate rebuild frames from your RDS export (one HSET per row):
#   HSET user:42 name ada email ada@example.com …
kevy-cli import -p 6004 --strict rows.resp        # ≥200k cmd/s
```

**Import first, declare indexes after** (the deferred-index rule):
backfill builds each index from existing rows at bulk speed —
orders of magnitude cheaper than paying the write hook per imported
row. Then run the phase-1 declaration script and wait for
`IDX.LIST` to report `state=ready` (details under
[Loading into an indexed keyspace](#loading-into-an-indexed-keyspace)).

- **Clamp:** `PREFIX.STATS` counts match `SELECT COUNT(*)` per table;
  `IDX.VERIFY` coercion/duplicate counts are the expected zeros; a
  sampled digest check — recompute a few hundred rows from the RDS
  and byte-compare against `HGETALL` — passes. If you stage through
  an intermediate kevy, `kevy-cli diff` proves the hop.
- **Rollback:** `kevy-cli delete-prefix` (or `FLUSHALL`) and redo.
  The RDS is still the truth.

### Phase 4 — Read cutover (canary, per endpoint)

Flip reads endpoint by endpoint, not big-bang. For each endpoint,
pick the consistency rung it needs
([availability.md](availability.md)): browse/list endpoints ride
rung 0 (async replica reads); "I just wrote this, show it back"
flows use `REPL.TOKEN`/`REPL.WAIT` (rung 1) or read the primary. A
single-node kevy has no ladder to think about — every read is
primary.

- **Action:** shadow-read first (serve from the RDS, also read kevy,
  compare and count mismatches), then shift traffic percentage.
- **Clamp:** mismatch counter at zero over a full traffic cycle
  (24h+); serving latency inside your SLO.
- **Rollback:** repoint reads at the RDS — instant, data untouched.

### Phase 5 — Write cutover + the rollback window

Flip writers to kevy, and **in the same deploy** start the reverse
mirror: a CDC consumer replaying kevy's feed back to the RDS as SQL
(`FEED.READ` → `UPDATE`/`INSERT`, cookbook recipe 13). The RDS is
now a warm standby that trails kevy by seconds; your rollback plan
is "repoint the app back", not "reverse-migrate".

Before the flip, seed sequence keys **above** the RDS high-water
mark (`INCRBY seq:order <rds_max_id>` on a fresh key) — the classic
auto-increment collision is phase 5's sharpest edge.

- **Clamp:** `kevy-cli diff`-style sampling between kevy and the
  mirrored RDS (digest a prefix, compare row samples); the business
  metrics you already alert on.
- **Rollback:** within the mirror window, repoint the app at the RDS
  (accepting the mirror lag, which you are measuring). The mirror
  consumer must handle `-FEEDRESYNC` (a kevy crash-restart bumps the
  feed generation): rebuild the affected prefix into the RDS, then
  resume — at-least-once + idempotent SQL makes this safe.

### Phase 6 — Decommission

When confidence hardens (weeks of clean diffs, one exercised
failover), stop the reverse mirror, take a final RDS dump for the
archive, and move the operational story fully onto kevy: snapshots +
AOF (+ feed-cursor recovery points) per
[persistence.md](persistence.md), replica/failover posture per
[availability.md](availability.md).

- **Clamp:** a restore drill from kevy's own artifacts (snapshot +
  feed replay) before the RDS is turned off — never after.

## Common pitfalls

- **DECIMAL through two write paths.** If the dual-write path stores
  `"10.00"` and the backfill stores `"1000"` (minor units), every
  digest check fails and — worse — reads disagree. Fix the format in
  phase 1 and make both paths share one serializer.
- **Auto-increment collision.** Forgetting to seed `seq:*` keys above
  the RDS max id hands out ids that already exist. Seed in phase 5,
  verify with one `GET seq:order` before opening writes.
- **Hot keys.** A kevy key lives on exactly one shard; the RDS row
  every request touched becomes a single-shard serialization point.
  Shard hot counters (per-user keys, block-allocated sequences) and
  let `KIND agg` do the read-side summing.
- **Large values.** Multi-hundred-KB rows work but make every hop
  heavier (export frames, replication, feed frames) and shift
  serving cost to the network path. Keep hot rows lean; store cold
  blobs in object storage with a pointer field, or split
  rarely-read columns into a sibling key.
- **TTL semantics.** `EXPIRE` with a non-positive TTL deletes the key
  immediately (Redis contract). Exported TTLs travel as absolute
  `PEXPIREAT` deadlines, so a slow migration doesn't extend
  lifetimes. Digests ignore TTLs (values only) — TTL correctness is
  checked by sampling `PTTL`, not by `diff`.
- **`KEYS`/`SCAN` habits.** Ops one-liners that did `SELECT COUNT(*)`
  should use `PREFIX.STATS` or `IDX.COUNT` — `KEYS pattern` is a
  full-keyspace gather and `SCAN MATCH` a full NON-incremental sweep — every SCAN call walks the entire keyspace on every shard and returns all matches with cursor 0, so the usual SCAN loop terminates after one round trip;
  neither is a serving path (and on a big keyspace, barely an ops
  path).
- **Index build racing reads.** Right after declarations, queries
  answer `-INDEXBUILDING`. Clients in the cutover window need the
  poll-and-retry loop (below), or declare in phase 3 where no reader
  is looking yet.

---

# The toolchain (`kevy-cli`)

```
kevy-cli export  -p 6379 --prefix user: dump.resp
kevy-cli import  -p 6004 --strict dump.resp        # ≥200k cmd/s
kevy-cli import  -p 6004 --resume dump.resp        # after interruption
kevy-cli digest  -p 6004 user:
kevy-cli diff    hostA:6379 hostB:6004 user: order:
kevy-cli copy-prefix   -p 6004 --rate 5000 user: staging:user:
kevy-cli delete-prefix -p 6004 --rate 5000 --dry-run tmp:
kevy-cli inspect -p 6004 user:
```

## Wire format

`export` writes a plain **RESP command stream** of rebuild frames —
`DEL` + `SET`/`HSET`/`RPUSH`/`SADD`/`ZADD`, plus absolute `PEXPIREAT`
for TTLs. That makes the file bidirectionally compatible with
`redis-cli --pipe`: kevy exports feed a Redis, and any RESP command
file feeds `kevy-cli import` — including one you generate yourself
from an RDS dump (the playbook's phase 3).

The leading `DEL` per key makes replay **rebuild from scratch** —
genuinely idempotent for every type it emits (an append verb like
RPUSH would otherwise double list content on re-import).

**It does not emit every type.** Strings, hashes, lists, sets and
sorted sets rebuild; **streams do not** — there is no rebuild verb for
them here, so an export leaves them behind. That is a real limitation
of this tool, not a property of your data, and it is now **reported by
name**:

```console
kevy-cli export: SKIPPED 1 key(s) of type 'stream' — nothing here
rebuilds that type, so they are NOT in this file
exported 4006 keys -> dump.resp
```

Until 4.1 the skip was silent: the key was counted as one that vanished
mid-walk, and a store with streams exported clean and short. If your
migration includes streams, move them separately — and check that line
before you trust the file.

**`copy-prefix` reads through the same rebuild set** and reports the
same way, for the same reason: a copy that leaves a type behind while
printing `copied N keys` is the same silence wearing a different verb.

## Consistency and resumability

- Export is per-key point-in-time under a SCAN walk (SCAN-class
  guarantees; keys written during export may appear in either state,
  vanished keys are skipped). No global snapshot — by design.
- Import pipelines 512 commands per batch and fsyncs a byte offset to
  `<file>.progress` after every batch. `--resume` restarts there;
  because frames rebuild, overlap is harmless. `--strict` aborts on
  the first server error; otherwise errors are counted and reported.
- `kill -9` mid-import is a gated scenario: resume converges to the
  same `PREFIX.DIGEST`.

## Verification

`PREFIX.DIGEST <prefix>` (server + embedded `prefix_digest`) returns
`[count, hex64]` — an order-insensitive checksum over canonical row
bytes (hash fields and set members sorted, zset by score bits then
member, list in order — list order IS identity). It is insensitive to
shard count and insert order, so it compares across topologies.
`kevy-cli diff A:port B:port prefix…` exits non-zero on any mismatch.

TTLs do not participate in the digest (they decay); values do.

## Bulk operations

`copy-prefix` re-keys every row under a new prefix via read + rebuild
frames (the server intentionally has no COPY verb; TTLs carried as
absolute deadlines). `delete-prefix` SCANs + UNLINKs. Both take
`--rate N` (token bucket, strict pacing from the first op) and
delete supports `--dry-run`.

## Loading into an indexed keyspace

### Waiting for index readiness

`IDX.CREATE` returns immediately and backfills in the background;
queries answer `-INDEXBUILDING` until done. The standard wait is
polling `IDX.LIST` — its `state` column flips from `building` to
`ready`:

```
until kevy-cli -p 6004 IDX.LIST | grep -A1 my_index | grep -q ready; do sleep 1; done
```

Backfill speed scales with DOCUMENT SIZE for text indexes: ~7s per
million small rows, but multi-KB bodies index at roughly 85s per
million (measured: 200k mail-sized bodies in 17s).

### Verifying a whole migration in one command

`kevy-cli diff` compares any number of prefixes across two live
servers in one call — prefer it over per-prefix digest pairs:

```
kevy-cli diff 127.0.0.1:6004 127.0.0.1:6005 msg: mbox: usr: tag: session:
```

### Large exports

The dump is uncompressed RESP text (fast, greppable). For 10GB+
keyspaces pipe through gzip — the format is stream-friendly:

```
kevy-cli export -p 6004 /dev/stdout | gzip > dump.kevy.gz
gunzip -c dump.kevy.gz | kevy-cli import -p 6005 --strict /dev/stdin
```

(`--resume` needs a real file for its .progress sidecar — decompress
to disk first if you want resumability.)

Create indexes **after** the bulk load: the index engine's backfill
builds from existing data at bulk speed (~7s per million rows
measured), which beats paying per-write hook maintenance a million
times. Order of operations, not a switch:

```
kevy-cli import -p 6004 dump.resp
kevy-cli -p 6004 IDX.CREATE users ON PREFIX user: FIELD age TYPE i64 KIND range
```

Gate: `bench/onrampgate.sh` (1M-row round trip, ≥200k cmd/s import,
kill -9 resume convergence, ±20% rate accuracy).
