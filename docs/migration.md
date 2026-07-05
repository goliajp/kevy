# Migration toolchain (`kevy-cli`)

Everything you need to move data into, out of, and between kevy
servers — and to prove the move was faithful.

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
file feeds `kevy-cli import`.

The leading `DEL` per key makes replay **rebuild from scratch** —
genuinely idempotent for every type (an append verb like RPUSH would
otherwise double list content on re-import).

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
queries answer `-ERR index building` until done. The standard wait is
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
