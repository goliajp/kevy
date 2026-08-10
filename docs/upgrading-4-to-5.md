# Upgrading from 4.x to 5.0

The short version: **stop the 4.1.1 server, start the 5.0 binary on
the same data directory.** No config changes are required; the 5.0
release gate replays this exact procedure (both directions, plus a
cold restore from a backup copy) before every tag.

## What carries over unchanged

- **Data files.** 5.0 opens 4.1.1 data directories as-is: AOF (v1 and
  v2 record formats), snapshots, `shards.meta`, segment directories.
  A v1-era AOF keeps appending v1 until its first rewrite upgrades it
  to the checksummed v2 envelope — same behavior as 4.1.
- **Downgrade window.** A 4.1.1 binary can re-open a data directory
  last written by 5.0 (the upgrade gate exercises this round trip).
  Do a clean shutdown before switching either direction.
- **Config files and flags.** Every 4.x key is accepted. Zero-config
  boot (`--port` + `--dir`) behaves identically.
- **Wire protocol and client contracts.** No RESP surface was removed
  or re-typed in this release.

## What behaves differently

### AOF writes leave the reactor thread (io_uring, default on)

On Linux with io_uring available, an AOF with `appendfsync everysec`
(or `no`) now queues appends onto the shard's ring instead of writing
synchronously on the reactor. This is the change behind the release's
tail-latency headline; durability semantics are unchanged (the
`everysec` crash window is still ≤ 1 s, verified by the crash gate in
both modes).

- `KEVY_AOF_OFFLOAD=0` restores the 4.x synchronous path.
- `appendfsync always` keeps the synchronous path by definition.
- The epoll reactor (kernels without io_uring) keeps the 4.x path.

### Rewrites under sustained write pressure defer instead of stalling

4.x would run an AOF rewrite whenever the growth rule fired and pay
whatever pause the disk demanded — under heavy ingest, multi-second
server stalls. 5.0 measures whether a rewrite can converge (append
rate vs. the rewrite's own progress) and, when it provably cannot,
**defers**: the log keeps growing, the server keeps answering, and the
rewrite retries after the next growth factor or on an explicit
`BGREWRITEAOF` (which is never gated). Expect larger transient AOF
sizes under sustained saturation — that is the trade, and it is the
honest one: disk is refundable, a stall is not.

### New housekeeping files beside the AOF

During and after rewrites you may briefly see `<aof>.rewrite` (the
image under construction), and `<aof>.trashN` (a hardlink that lets
the swap free the old log's gigabytes off-thread). Both are cleaned
automatically; an orphan left by a crash is reclaimed by the next
rewrite. Do not back these up; a backup is the data directory minus
`*.rewrite` / `*.trash*` — or simpler, copy everything and let 5.0
sort it out on boot (the restore drill in the disk gate does exactly
that).

## Recommended procedure

1. Take a backup: clean shutdown (or `BGSAVE` + copy), then copy the
   data directory.
2. Stop 4.1.1. Start the 5.0 binary on the same directory and config.
3. Verify: `INFO persistence` (aof enabled, rewrites proceeding),
   `DBSIZE` against expectations, your application's own smoke.
4. Rollback, if needed: stop 5.0 cleanly, start 4.1.1 on the same
   directory.
