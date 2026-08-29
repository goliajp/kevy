# RFC: v1.14 review PLAUSIBLE fixes (2026-06-11)

Four review leftovers, one feature branch (feature/review-plausibles).

## 1. Server reshard is now crash-idempotent (reshard.rs)

Old order renamed sources to `.premigration` *before* writing the new
snapshots — a crash in that window left the dir empty (data only hand-
recoverable from backups). New order: write snapshots under `.reshard`
temp names → durable `reshard.journal` (= commit point) → rename sources
away → finalize temps → write meta → drop journal. Crash before the
journal: old layout intact, stale temps cleaned on the next attempt.
Crash after: `recover_journal` rolls the migration forward on startup
(every step an idempotent rename). A torn journal (crash mid-journal-
write) is discarded — the commit point was never reached. An unreadable-
but-present journal aborts startup rather than re-resharding over
partially-renamed sources.

Note: the embedded reshard (kevy-embedded/shard.rs) has a structurally
similar window (sources renamed before per-shard AOF rewrites). Not in
this review's scope; candidates for the steel-dedup pass (③) which would
unify both reshard implementations.

## 2. XREADGROUP cross-shard gather partial failure — documented semantics

Upstream Redis pre-validates all keys/groups before delivering. Kevy's
shards execute independently: shard A may record PEL deliveries before
shard B reports NOGROUP, so the client sees an error while A's deliveries
stand. They are recoverable exactly like a client-crash mid-read
(XPENDING / XAUTOCLAIM). Pre-validation would cost an extra cross-shard
round-trip per multi-stream XREADGROUP. Decision: keep, document
(reduce.rs `finalize_xread_gather` doc comment).

## 3. Embedded n==1 / server dir interop (kevy-embedded/shard.rs)

Reality check on the review claim: the embedded defaults have been
`dump-0.rdb` / `aof-0.aof` since the crate was created, so default-named
single-shard dirs were already server-readable via filename inference.
The real residual gaps, both fixed:

- **Silent partial load**: a meta-less multi-shard dir (pre-meta server)
  opened at n==1 was mistaken for the single-file layout and loaded
  shard 0 only — (k-1)/k of the keyspace silently dropped. Now the file
  names are inferred (`infer_files_n`, mirroring the server runtime) and
  the dir is migrated whole.
- **No meta on the n==1 path**: single-shard opens never recorded
  `shards.meta`. Default-named dirs now do; custom
  `with_aof_filename`/`with_snapshot_filename` names stay meta-less by
  design (a meta would point a server at files that don't exist) —
  custom names are a documented interop opt-out (config doc comments).

## 4. io_uring dead-conn block waiters (uring_reactor.rs)

`uc.closing = true` (EOF / write error / protocol error) only took
effect at `uring_reap_closed`, which runs on a 1/16-iteration throttle —
a parked BLPOP/XREAD waiter on a dead conn stayed live for up to 16
iterations and could consume a push (e.g. an LPUSH element) meant for a
live client. New `uring_mark_closing` cancels local block waiters +
cross-shard arbiter registrations eagerly at all three closing sites;
the full teardown still happens in the throttled reap.
