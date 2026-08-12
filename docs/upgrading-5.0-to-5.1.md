# Upgrading from 5.0 to 5.1

The short version: **stop the 5.0 server, start the 5.1 binary on the
same data directory.** No config changes, no wire changes, no client
changes. The release gate replays this procedure in both directions,
including a mixed-version primary/replica pair, before every tag.

**Upgrade sooner rather than later if you use compression or value
logging.** 5.0's encoder can write a compressed frame that 5.0's own
decoder rejects after compaction — a cold value that reads back as
corrupt. 5.1 fixes the encoder and teaches the decoder to rescue the
frames 5.0 already put on disk. Details below.

## Why this release exists

- **A 5.0 read hazard is repaired.** When a dictionary carried a
  shared Huffman table, `kevy-compress` could tag a literal-only frame
  as if it used dictionary matches. Nothing detects it at write time
  (the CRC is over the bytes that were written), so the failure
  surfaces later, on the compaction path, as a decode error on a value
  that was stored successfully. 5.1 fixes the tag AND adds an explicit
  compatibility arm that decodes the mis-tagged frames 5.0 has already
  written. Frames written by 5.1 stay readable by 5.0.
- **Giant collections stop stalling their shard.** Lists, hashes,
  sets and sorted sets past ~16k elements now copy on write at
  element granularity. A write that lands while a rewrite or snapshot
  pins the value clones one segment (about a millisecond, independent
  of collection size) instead of the whole value — which used to cost
  0.35–9.5 s and briefly double that collection's memory.
- **`appendfsync always` stops blocking the reactor.** Both reactors
  now group-commit: on io_uring the reply is gated on the fsync
  completion, and on epoll/kqueue a per-shard writer lane does the
  same. Measured on ext4, 50 concurrent connections: 353 → 8,273
  writes/s on io_uring, 478 → 10,540 on epoll. The durability
  contract is unchanged — a reply still means the write is on disk.
- **Failover convergence is fixed.** A replica retargeted at a newly
  promoted primary could stay attached and idle forever: link up,
  heartbeats flowing, no resync in progress, and the post-failover
  writes never arriving. See the replication section below.

## What carries over unchanged

- **Data files.** 5.1 opens 5.0 directories as-is, and 5.0 re-opens a
  directory last written by 5.1 (both directions are gated). Do a
  clean shutdown before switching either way.
- **Wire protocol, commands, and client contracts.** Nothing was
  removed or re-typed. Existing clients need no change and no rebuild.
- **Config files and flags.** Every 5.0 key is accepted, with the same
  defaults.

## What behaves differently

### A replica that reconnects gets one snapshot resync

A replica whose replication cursor has no continuity claim — the state
after any retarget, `REPLICAOF`, or a version change that moves the
feed's generation — is now answered with a full snapshot rather than a
replay from the start of the current offset space. This is the only
answer that converges a replica whose contents the primary cannot see:
a stream of frames can add and overwrite keys, but it can never remove
a key the replica holds and the primary does not.

What you will observe once, per shard, on the first connection after
the upgrade:

```text
kevy: replica fd 42 generation 6843605247850762 != feed generation
      4198793490690396 (sent_offset 0); shipping snapshot
```

That is expected and self-healing. It also means a replica's local
divergence is discarded rather than carried forward, which is the
documented contract for a forked history.

The bug this closes: promotion opens writes the instant the election
resolves, but each shard fences its replication offsets on its own
tick. A write accepted inside that window was destroyed by the fence,
and a replica attaching afterwards was told it was already caught up —
so the write existed in the primary's keyspace and in no stream, with
the link reporting perfect health.

### `appendfsync always` shares fsync rounds

Under `always`, concurrent connections now share fsync rounds (group
commit) instead of each paying its own. A single sequential client
sees a modest slowdown — its write now waits for a queued fsync and
its completion rather than an inline one — while any concurrent load
gains an order of magnitude. `KEVY_AOF_OFFLOAD=0` restores the classic
synchronous path on either reactor if you need the old shape.

### Feed generations are random identities

A feed generation is now a random 53-bit history identity rather than
a counter, so two nodes can never call two different histories by the
same name. Anything that printed or stored a generation (`REPL.TOKEN`,
`REPL.WAIT`, `FEED.TAIL`) will show large unordered numbers. They are
identities: compare them for equality, never for order.

## Recommended procedure

1. Take a backup: clean shutdown (or `BGSAVE` + copy), then copy the
   data directory.
2. Upgrade replicas first, then the primary. A mixed pair works in
   both directions; each replica resyncs once when its cursor's
   generation stops matching.
3. Verify: `INFO replication` on each replica (link up, offsets
   advancing), `INFO persistence`, `DBSIZE` against expectations, and
   your application's own smoke test.
4. Rollback, if needed: stop 5.1 cleanly, start 5.0 on the same
   directory. Values written by 5.1 remain readable.
