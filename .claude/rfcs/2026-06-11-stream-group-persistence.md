# RFC: consumer group / PEL persistence (2026-06-11)

## Gap (pre-existing, found in v1.14 review)

Three sites drop consumer groups + PEL:

1. **Snapshot** (`kevy-persist/lib.rs` `OP_STREAM`): payload has entries + scalars only.
2. **AOF rewrite** (`rewrite_fmt.rs`): emits per-entry `XADD` only — no groups, and also
   loses `last_id`/`entries_added`/`max_deleted_id` when the tail was XDEL'd; an
   **empty stream** (all entries deleted, or groups-only) vanishes entirely.
3. **Reshard in-memory redistribution** (`Store::load_value`, keyspace.rs): the
   `Value::Stream` arm copies entries + scalars, not groups.

## Design

### D1 — `XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]` (new command)

Redis-7-compatible. Needed by the rewrite to restore stream scalars; also a real
compat-surface gap. Errors mirror Redis: missing key → "requires the key to exist";
last-id below top entry → "smaller than the target stream top item". Write verb
(AOF-propagated, keyspace-notify class `t`), non-growing.

### D2 — snapshot format v4

`VERSION = 4`; loader accepts v2 (relative TTL), v3, v4. New const
`VERSION_ABSOLUTE_TTL = 3` keeps the TTL-interpretation check correct.
`OP_STREAM` payload appends after the entries block:

```
u32 n_groups, per group:
  bytes name | u64 last_delivered.ms | u64 last_delivered.seq
  u32 n_consumers, per consumer: bytes name | u64 last_seen_ms
  u32 n_pel, per entry: u64 ms | u64 seq | bytes consumer | u64 delivery_time_ms | u32 delivery_count
```

Full fidelity: tombstone PEL entries (stream entry XDEL'd while pending) and
consumer `last_seen_ms` survive. `pel_count` is recomputed at load. A PEL owner
missing from the consumer roster (corrupt/hand-built file) is created on load —
by-argument input never panics.

Exchange types: `LoadedGroup` / `LoadedPelEntry` in kevy-store (`stream/load.rs`)
with `StreamData::export_groups` / `import_groups`; `Store::load_stream` gains a
`groups` parameter. `load_value`'s Stream arm uses export/import → fixes reshard.

### D3 — AOF rewrite emission (per stream, after the XADD sequence)

1. Empty stream with `last_id != 0-0`: `XADD key MAXLEN 0 <last_id> x x` recreates
   the key with the right `last_id` (the trim wipes the dummy entry).
2. `XSETID key <last_id> ENTRIESADDED <n> MAXDELETEDID <id>` — emitted only when the
   natural replay state would differ.
3. Per group: `XGROUP CREATE key g <last_delivered> MKSTREAM` (MKSTREAM covers the
   groups-on-virgin-empty-stream case), then `XGROUP CREATECONSUMER` per consumer,
   then per live PEL entry `XCLAIM key g <owner> 0 <id> TIME <delivery_time>
   RETRYCOUNT <count> FORCE JUSTID` — full delivery_time/count fidelity (same
   technique as Redis's own AOF rewrite).

### Trade-offs (documented, not bugs)

- **Tombstone PEL entries are dropped by AOF rewrite** (kevy's XCLAIM purges
  PEL rows whose entry is gone, so they can't be re-created by command replay).
  They are unservable anyway — XREADGROUP replay skips them, XAUTOCLAIM reaps
  them; only XPENDING visibility is lost across a rewrite+restart. The snapshot
  path (v4) preserves them fully.
- **Consumer `last_seen_ms` is not preserved by AOF rewrite** (CREATECONSUMER
  stamps replay-time). Matches upstream behavior; snapshot path preserves it.

## Acceptance

- BGREWRITEAOF → restart: groups, consumers, PEL (delivery_time/count) intact.
- SAVE/snapshot → restart: ditto, including tombstone PEL + last_seen_ms.
- Reshard round-trip: ditto.
- Empty-stream `last_id` + groups survive rewrite (new — was a silent data loss).
- v3 snapshot still loads (backward compat).
- perfgate PASS (all touched paths are cold; dispatch gains one match arm).
