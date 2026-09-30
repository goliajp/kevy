# Upgrading from 6.4 to 7.0

The short version: **a wire client needs no code change, and the data
directory opens as it is.** Four things need a look before you swap the
binary:

- `used_memory` reads higher for the same data — about 1.5 times on a mix
  of strings and hashes — because it now counts what the allocator holds.
  A `maxmemory` set close to what 6.4 reported starts evicting sooner
  ([§4](#4-used_memory-reads-higher-for-the-same-data)).
- With replicas, upgrade the primary first, or restart each replica once
  after the primary runs 7.0; otherwise a replica goes without indexes,
  views and tables until the next catalog command
  ([§2](#2-replicated-setups-upgrade-the-primary-first)).
- Going back to 6.4 loses the index, view and table catalog unless you
  kept the side files from before the upgrade, and an embedded store
  killed under 7.0 needs one clean open and close under 7.0 first
  ([§1](#1-going-back-to-64-what-the-directory-may-hold)).
- A Rust caller needs edits; the compiler names each one
  ([§13](#13-the-rust-api)).

The major version is for the last three kinds of change: every public
Rust API of the workspace now follows the Rust API Guidelines, which
changed signatures in most crates; `kevy-config`'s structs gained public
fields, which breaks a struct literal (§10); the Go module moved to
`github.com/goliajp/kevy-go/v7` (§14); and a directory 7.0 has written
holds files and log frames 6.4 does not understand (§1).

7.0.0 is about four things: an embedded store that keeps every write a
killed process returned and opens and closes faster; global indexes,
spread over the shards by value; index rows that cost a quarter or less
of what they did; and encrypted links — between nodes, and on a second
client port — all off unless configured
([encrypted-links.md](encrypted-links.md)). It also fixes defects that
had been losing or changing data, some since 1.x; they are in
[§16](#16-defects-fixed-that-lost-or-changed-data).

```toml
kevy-embedded = "7.0.0"
```

## TL;DR — what to do

| If you… | What changes | § |
|---|---|---|
| run the server, or talk to kevy over the wire | swap the binary; check `maxmemory` | 4 |
| set `maxmemory`, or run a tiered server | `used_memory` reads about 1.5× higher for the same data; a tiered server also refuses growing writes while the process holds more than its budget | 4 |
| run replicas | upgrade the primary first, or restart each replica after it; a replica's own index declarations are dropped | 2 |
| send `BLPOP`, `BRPOP`, `RENAME`, `RENAMENX` or a catalog command to a replica | refused with `READONLY` | 3 |
| may downgrade to 6.4 | open and close cleanly with 7.0 first; keep the catalog side files from before the upgrade | 1 |
| set `MAXMEM` on an index, or size a tiered store near its index floor | index sizes read differently: smaller for large indexes, about 1.8 KB a shard at least | 5 |
| parse `IDX.LIST` or `IDX.DESCRIBE` positionally | each gains a `partitioning` pair | 6 |
| read an embedded store's change feed or AOF | an `MSET` arrives as one frame per shard | 7 |
| start two servers on one port by accident | the second one now refuses to start | 8 |
| run the io_uring reactor | 16 MiB of receive buffers a shard instead of 64; a shard polls for 200 µs after forwarded work | 9 |
| build `kevy_config` structs with struct literals | new fields to name, or `..Default::default()` | 10 |
| implement `kevy_rt::Commands` | new methods, all with defaults | 11 |
| script `kevy-cli doctor`, `export`, `sql compile` … as bare words | put the tool after `--kevy` | 12 |
| use a kevy crate as a Rust library | most signatures changed; the compiler names each one | 13 |
| use the Go module, or match a binding's read-only error text | import `/v7`; the text gained its closing period | 14 |
| use `XAUTOCLAIM`'s cursor, an id like `5-`, or `EXPIRE` on a key about to lapse | they now behave as in Redis | 15 |
| want an index read to reach fewer shards | declare it global | [indexes](indexes.md#global-indexes-partition-global) |

---

## What carries over

Measured against the 6.4.0 release binary (built from the `v6.4.0` tag)
and 7.0.0, two shards each, on Linux with the io_uring reactor, and for
the embedded store also on macOS. The data set held strings, a key TTL,
a hash with a field TTL, a list, a set, a sorted set, a stream with a
consumer group (one consumer made by a read, one by `XGROUP
CREATECONSUMER`), two indexes, a table and a materialized view, with a
`BGSAVE` part-way so that some writes lived only in the log.

| Case | Keys, TTLs, field TTLs, streams | Indexes, views, tables |
|---|---|---|
| 6.4.0 directory opened by 7.0 | all there | all there; the side files are moved into the log and removed |
| 7.0 directory opened by 6.4.0, from the log | all there, except a consumer made only by `XGROUP CREATECONSUMER` | none |
| 7.0 directory opened by 6.4.0, from a snapshot and the log | all there | none |
| 7.0 reopening after 6.4.0 wrote to it | all there, including what 6.4.0 wrote | back, unless 6.4.0 rewrote the log (§1) |
| 6.4.0 primary → 7.0 replica | full sync and live stream converge | none; see §2 |
| 7.0 primary → 6.4.0 replica | full sync and live stream converge; a consumer made by `XGROUP CREATECONSUMER` arrives only at the next full sync | none |

6.4.0 skips the new frames without logging anything. **A backup is a
copy**: a copied directory serves what the original did, including one
copied after a process was killed (§1).

## 1. Going back to 6.4: what the directory may hold

### After a crash or a kill

A killed process used to lose the writes still waiting in a user-space
buffer. 7.0 puts every append in memory the kernel owns the moment the
append returns:

- **On Apple platforms**, the AOF itself is mapped, with a preallocated
  tail (4 MiB, doubling up to 64 MiB) that appends are copied into. While
  the store is open the file is longer than its records, and the rest is
  zeros; a clean close truncates it. A killed process leaves the zeros,
  and the next open trims them.
- **Elsewhere**, appends go to a staging ring first — a small mapped file,
  `aof-<i>.aof.stage` (4 MiB by default), drained into the AOF on every
  tick. The next open replays whatever a killed process left in it — in
  the directory itself, or in a copy of it taken after the kill, as a
  backup is.

6.4 knows neither. Measured with an embedded writer killed with
`SIGKILL` in the middle of a stream of writes:

| Directory right after the kill | 6.4.0 opens it | 7.0 opens and closes it, then 6.4.0 opens it |
|---|---|---|
| macOS, mapped AOF (the default there) | every returned write is there; 6.4.0 reports a corrupt tail and moves the zeros (20 MB in this run) to an `aof-<i>.aof.corrupt-quarantine.<ts>` file | every returned write, no warning, no quarantine file |
| Linux, staging ring (the default there) | the writes still in the ring are lost (9,992 of 1,303,496 in this run); 6.4.0 leaves the `.stage` file where it is | every returned write |

So **after a crash or a kill, open the directory once with 7.0 and close
it cleanly before going back to 6.4**: that drains the ring and truncates
the tail, and leaves files 6.4 reads as it always did. Off Apple
platforms the drained `aof-<i>.aof.stage` stays in the directory; 6.4
ignores it. If 6.4 has already opened a killed directory and written to
it, 7.0 does not replay the stale ring over those newer writes: it keeps
what 6.4 wrote, and the writes that were in the ring stay lost.

`Config::with_stage_ring(0)` and `Config::with_mapped_aof(false)` turn
the two off; `AppendFsync::Always` uses neither. With both off, a killed
process loses what was in the buffer, as in 6.4 (measured: 44 to 80
writes in these runs).

### The catalog and consumer contacts

Two more things in the log are new, for a server and an embedded store
alike:

- A stream group's consumer contacts are recorded as internal
  `XINTERNAL.CONSUMERSEEN` frames. 6.4 skips them, and so loses a
  consumer made only by `XGROUP CREATECONSUMER` when it rebuilds the
  group from the log; a consumer that is in a snapshot survives.
- The index, view and table catalog is no longer kept in
  `index-catalog.meta`, `view-catalog.meta` and `table-catalog.meta`.
  7.0 records each change in the log as one internal `XINTERNAL.CATALOG`
  frame carrying the whole catalog, keeps the current one in every
  snapshot, and on its first start on a 6.4 directory moves the catalog
  out of the three files and removes them.

What that means for going back, measured:

- 6.4.0 opens a directory 7.0 wrote with every key and no indexes, views
  or tables, from the log or from a snapshot.
- Whatever 6.4 then writes, 7.0 reads, and the catalog comes back with it
  — unless 6.4 rewrote the log (`BGREWRITEAOF`), which drops the frames;
  7.0 then opens with the catalog of the newest snapshot, or none.
- Copying the three side files from before the upgrade back into the
  directory before 6.4 first opens it gives 6.4 its catalog back.

So before going back, keep a copy of the three files from before the
upgrade and put them back first, or declare the catalog again on 6.4.

## 2. Replicated setups: upgrade the primary first

A replica follows its primary's keys across versions in both directions.
The catalog does not cross: a 6.4 primary never sent its catalog to
replicas, and 6.4 skips the frame a 7.0 primary sends. Measured with a
6.4.0 primary holding two indexes, a table and a view, and a 6.4.0
replica that had declared an index of its own:

- **Primary first.** While the replica still runs 6.4.0 it keeps the
  index it declared itself and receives none of the primary's. At its
  first start as 7.0 it takes the primary's whole catalog in its full
  sync, and the index it declared itself is gone. Its
  `index-catalog.meta` is left in the directory.
- **Replicas first.** A 7.0 replica of a 6.4.0 primary has no indexes,
  views or tables — and refuses to declare any (§3) — so every index
  query on it fails with `no such index` until the primary runs 7.0.
  And when the primary then restarts as 7.0, a replica that stays
  connected still has no catalog: the primary's import is not sent to it.
  It gets the catalog at the next catalog command on the primary
  (`IDX.CREATE`, `TABLE.DECLARE`, …) or when the replica restarts.

So upgrade the primary first, then each replica. If the replicas already
run 7.0, restart each one once after the primary does. A replica that
declared indexes of its own under 6.4 needs them declared on the primary
instead ([replication](replication.md#trade-offs-and-limits)).

## 3. A replica refuses more writes

A read-only replica answers these with `-READONLY You can't write against
a read only replica.`, where 6.4 ran them against its own keyspace and let
it drift from the primary:

- `BLPOP`, `BRPOP`, `RENAME` and `RENAMENX` (6.4.0 measured: a `BLPOP`
  sent to a replica popped the element from the replica's copy of the
  list);
- every catalog command: `IDX.CREATE` / `DROP` / `REBUILD`, `VIEW.CREATE`
  / `DROP` / `REBUILD`, `TABLE.DECLARE` / `ENSURE` / `REPLACE` / `DROP`.

An `EVAL_RO` script can no longer call the first four either. An embedded
replica refuses the catalog methods with `KevyError::ReadOnly`. Declare
indexes, views and tables on the primary; they reach every replica.

## 4. `used_memory` reads higher for the same data

6.4 charged every key a flat 96 bytes for its place in the keyspace table
and undercharged hashes (the box around a hash's table was never
counted, and each slot was charged 32 bytes where it takes 49). 7.0
charges the keyspace table and each hash the bytes the allocator holds
for them, so `used_memory`, `MEMORY USAGE`, `maxmemory` eviction and the
tiered store's demotion all see the larger, real figure. The process
does not use more memory; more of it is counted.

Measured on the same 250,000 keys (200,000 strings of 32 bytes, 50,000
hashes of four fields, one of 200 bytes) on two shards:

| | 6.4.0 | 7.0 |
|---|---:|---:|
| `used_memory` | 69,200,000 | 103,543,040 |
| process RSS | 220.8 MB | 207.7 MB |
| `MEMORY USAGE` of a string | 128 | 200 |
| `MEMORY USAGE` of a hash | 872 | 1,272 |

A `maxmemory` sized from 6.4's `used_memory` therefore starts evicting at
about two-thirds of the data it held before. Size it against RSS, or
raise it by the ratio you measure on your own data.

A tiered server (`--tiering-budget`) also watches its resident memory
now. If live memory stays above the budget × 1.05 for two readings in a
row, every shard refuses writes that grow the data with `-OOM command not
allowed when the process holds more memory than the tiering budget
allows`, until it falls back. `INFO # Tiering` gains
`tier_rss_line_bytes`, `tier_refusing_writes`, `tier_live_bytes` and
`tier_overhead_bytes`.

## 5. Index sizes are reported as they are

The `bytes` of `IDX.LIST`, `IDX.VERIFY` and `TABLE.VERIFY` was `value +
key + 48` per row, an estimate. It is now what the index's leaves hold,
and an index holds far less than it did: an `i64` index over keys like
`row:<n>` reports 16–25 bytes a row where 6.4 reported 67, and holds a
quarter or less of what 6.4's did. The same figure feeds an index's
`MAXMEM` and the tiering reservation for indexes, so:

- an index whose `MAXMEM` was sized for 6.4 holds several times the rows
  before `-INDEXOVERBUDGET`;
- a tiered store keeps more data hot beside the same indexes.

Two cases read higher than 6.4's estimate:

- A small index: every shard that holds a row of it holds at least one
  1,784-byte leaf. Measured: one row reported 59 bytes on 6.4.0 and 1,816
  on 7.0; three rows over two shards, 186 and 3,632. A `MAXMEM` of a few
  kilobytes can now refuse the build.
- An index with long string values over non-numeric keys, once rows
  arrive in random order, since leaves then run 60–70% full until the
  background repack reaches them.

Check budgets set close to the line against `IDX.LIST` on a loaded
sample. The per-row formula is in
[indexes.md](indexes.md#consistency--cost-model).

## 6. `IDX.LIST` and `IDX.DESCRIBE` gain a pair

- Each `IDX.LIST` row ends with `partitioning local|global`; a global
  index adds `partitions`, `max_entries` and `mean_entries`.
- `IDX.DESCRIBE` names the `partitioning` right before `declaration`, which
  stays the last pair.

A client that reads these replies by key sees new keys it can ignore; one
that counts positions does not.

## 7. An embedded `MSET` is one frame per shard

An embedded store used to set each pair of an `MSET` under its own lock
and log it as its own `SET`. It now sets a shard's pairs under one lock
and logs them as one `MSET` frame, so a crash keeps each shard's share
whole or not at all. The AOF, a replica and the change feed see `MSET`
frames where they saw one `SET` per key; a feed consumer that handles only
`SET` should handle `MSET` too, as it already had to for a server.

## 8. A port already held is refused

Every shard listens with `SO_REUSEPORT`, which let a second kevy started by
the same user on the same port join the first one's listeners: both ran,
each took a share of the connections, and writes seemed to vanish from
whichever one a client read back. A port in use now stops startup with
`Address already in use`, as Redis does.

## 9. io_uring: a smaller receive ring, and polling after forwarded work

- Each shard on the io_uring reactor kept 4,096 receive buffers of 16 KiB
  — 64 MiB a shard, all of it resident once traffic had cycled through
  it. The count is now `[advanced] recv_buffers` and defaults to 1,024,
  16 MiB a shard. A ring that runs dry is not an error; the receive is
  re-armed. Set it back to 4096 if you measured a need for it
  ([tuning.md](tuning.md)).
- After a batch of work forwarded from other shards, a shard slept for
  200 µs, deaf to new input, and a client that paused between pipelined
  batches waited it out each time. It now keeps polling for those 200 µs
  instead, which costs up to 200 µs of one core after each burst of
  forwarded work before the shard parks. On a machine you share with
  other processes this shows as CPU time.

## 10. `kevy-config`: new fields on the section structs

The encrypted links and the proxy-friendly cluster added settings, and a
setting is a public field:

| struct | new fields |
|---|---|
| `Config` | `secure` (a `SecureSection`: `private_key_file`, `listen_port`, `client_keys`, `cluster_port_base`, `announce_cluster_port_base`) |
| `ClusterSection` | `announce_ip`, `announce_port_base`, `secure`, `peer_keys` |
| `PeerEntry` | `repl_port_base` |
| `ReplicationSection` | `secure`, `upstream_key`, `replica_keys` |
| `AdvancedSection` | `recv_buffers` |

A struct literal that names every field stops compiling:

```rust
// 6.4
let repl = ReplicationSection { role, upstream, listen_port_base, /* … every field */ };
// 7.0
let repl = ReplicationSection { role, upstream, listen_port_base, ..Default::default() };
```

Every struct above has a default except `PeerEntry`, whose new field is
`repl_port_base: None` for the old behaviour. A config built by
`Config::load` or `Config::from_toml_str` needs nothing: every new setting
defaults to off, and a `kevy.toml` written for 6.4 loads unchanged.

## 11. For Rust embedders of the runtime

`kevy_rt::Commands` gains `take_ext_out`, `apply_ext` and
`extension_targets`, all with defaults that keep 6.4's behaviour. They
carry messages between shards (a write replies only once they are
applied) and let an extension read name the shards it needs. The global
index is built on them. It also gains `snapshot_aux`, `load_snapshot_aux`
and `on_restored`, with defaults that keep nothing beside the keyspace:
they let a command set keep state of its own in every snapshot and
rewritten log, and settle it once every shard has restored, which is how
the index, view and table catalog is kept (§1).

## 12. kevy-cli: tools answer behind `--kevy` only

kevy-cli 6.4 ran its tools as bare words — `kevy-cli doctor -p 6004`,
`kevy-cli export …`, `kevy-cli sql compile f.sql --apply --url h:p` — and
through 6.x printed a line naming the `--kevy` form. In 7.0 a bare word is
a server command, as it is in redis-cli, so the old line sends `DOCTOR` to
the server and prints its error. Move the tool behind `--kevy`, and the
connection options in front of it:

```sh
# 6.4
kevy-cli doctor -p 6004
kevy-cli sql compile schema.sql --apply --url h:p
# 7.0
kevy-cli -p 6004 --kevy doctor
kevy-cli -h h -p p --kevy sql compile schema.sql --apply
kevy-cli -p 6379 --kevy diff 127.0.0.1:6380 user:        # the first server is the session's
```

`digest <prefix>`, and `backup`/`restore` in their file shapes, are
`--kevy digest|backup|restore`. The library functions that served the old
forms (`route_tool`, `run_doctor_cli`, `run_shadow_cli`, `run_lint_cli`,
`run_backfill_keys_cli`) are gone with them.

## 13. The Rust API

Every public Rust API of the workspace now follows the
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/), so
code that uses a kevy crate as a library will not compile unchanged. The
changes follow a few rules, and the compiler points at each place:

- a `bool` parameter is an enum named for its meaning —
  `store.copy(a, b, true)` is `store.copy(a, b, CopyMode::Replace)`;
- a struct or enum the library may grow is `#[non_exhaustive]`: build it
  from `Default` or its `with_*` builders instead of a struct literal, and
  give a `match` on it a `_` arm;
- a type with invariants keeps its fields private and offers methods of
  the same name;
- a free function that took a type first is a method on that type;
- an error is a type that implements `std::error::Error`, never a
  `String`; where its text reached the wire, `as_wire()` or `to_wire()`
  returns the same text;
- a (generation, offset) pair of the change feed and replication is one
  `FeedPosition`.

For an embedded store the common edits are these:

```rust
// 6.4
store.copy(b"src", b"dst", true)?;
store.linsert(b"l", true, b"c", b"b")?;
let (generation, offset) = store.changes_tail()?;
let batch = store.changes_since(generation, offset, 100, &[])?;
let dropped: bool = store.idx_drop(b"by_age");
// 7.0
store.copy(b"src", b"dst", CopyMode::Replace)?;
store.linsert(b"l", InsertPosition::Before, b"c", b"b")?;
let tail: FeedPosition = store.changes_tail()?;
let batch = store.changes_since(tail, 100, &[])?;  // batch.next is the next position
let dropped: bool = store.idx_drop(b"by_age")?;     // a closed store or a replica is an error now
```

`keys_iter` returns a `KeysIter` that holds one page rather than a copy
of the keyspace. [rust-api-7.0.md](rust-api-7.0.md) lists every change,
crate by crate, as old → new.

## 14. Bindings: the Go module path and the read-only error text

- **Go.** The module is `github.com/goliajp/kevy-go/v7`. Change the
  import path and `go get` it; nothing else in the Go API changed.

  ```go
  // 6.4
  import kevy "github.com/goliajp/kevy-go/v6"
  // 7.0
  import kevy "github.com/goliajp/kevy-go/v7"
  ```

- **Read-only error text.** The read-only error the C++, C#, Go, Python,
  Tauri and TypeScript bindings construct themselves now reads `READONLY
  You can't write against a read only replica.`, exactly the server's
  reply. Five of them lacked the closing period, and the Tauri plugin said
  `READONLY the store is a read-only replica`. Code that matches the whole
  string needs the new text; code that matches the `READONLY` prefix or
  the error type needs nothing.
- **Android.** `KevyDB.mget` makes one native call instead of running an
  `MGET` through the command path, and `KevyDB.mset(vararg pairs)` is new.
  The results are the same.
- **Tauri.** The plugin's Rust `Error` gains an `Other` variant (its
  `kind()` is `"Other"`), a pub/sub event's `count` is an `i64`, and an
  event kind the plugin does not know is no longer forwarded to the
  webview. The JavaScript API is unchanged.
- Every other door (Node, TypeScript, Electron, Expo, Nitro, Flutter,
  Python, C#, Java, Swift, C++, the wasm package) keeps its API. The
  engine changes an embedded door can observe are those of §1, §7 and
  §15, and the stream and geo commands, which the embedded engine now
  serves through the generic command path (a `BLOCK` read is refused
  there); its downgrade follows §1.

## 15. Replies that changed

Each of these now answers as Redis does, or lost text that was a
mistake:

- `XRANGE`, `XREVRANGE` and the other commands that take a stream id
  refuse `5-`, an id with nothing after the dash, with `ERR Invalid stream
  ID specified as stream command argument`. 6.4 read it as `5-<largest
  sequence>`.
- `XAUTOCLAIM`'s cursor is the id of the next pending entry, and `0-0`
  once the list is done; 6.4 returned the last scanned id plus one, so a
  call that reached the end answered a cursor instead of `0-0`. One call
  looks at no more than `COUNT × 10` entries, as in Redis, where 6.4
  scanned the whole list. A loop that calls until the cursor is `0-0`
  works on both, with one call fewer on 7.0.
- `EXPIRE`, `PEXPIRE`, `EXPIREAT` and `PEXPIREAT` with a non-positive
  TTL on a key whose deadline passes during the command answer 0 and
  record nothing; 6.4 answered 1 and recorded a removal.
- `INFO replication` reports `repl_port_base`.
- An embedded store's `table_declare`, `table_replace` and
  `table_verify_report` refuse with `-ERR …`, not `-ERR ERR …`.
- The refusal of a `TABLE.DECLARE … WINDOW` that cannot be served lost 17
  stray spaces in the middle of its text.
- Through `Store::dispatch_argv`, the path every language binding uses:
  `INCRBYFLOAT` answers with the stored value's own digits, as the server
  does; a malformed write on a closed store or a replica is refused as
  closed or `READONLY` before its arguments are checked; and a replica's
  `READONLY` reply ends with a period, as the server's does.

## 16. Defects fixed that lost or changed data

Each of these could lose a write, a deadline or a key, or change a value,
without an error; the version is the first release that had it:

- `COPY … REPLACE` (since 6.0.0) and a `RENAME` across shards (since
  5.0.0) replayed and replicated into what the destination held, merging
  a hash's fields or a list's elements into the old value;
- `BLPOP` and `BRPOP` pops never reached the AOF (since 1.4.0) or a
  replica (since 1.18.0), so the popped elements came back after a
  restart; a served waiter of `BZPOPMIN`, `BRPOPLPUSH` or `XREADGROUP …
  BLOCK` was not recorded either;
- writes made after a `BGSAVE`, on macOS and on Linux without io_uring
  (since 5.1.0);
- writes made during an AOF rewrite's final swap (since 5.0.0 on
  io_uring, 5.1.0 on epoll and kqueue);
- writes made after a process died inside a transaction, at the restart
  after next (since 4.0.0);
- a hash field's own TTL, after a background AOF rewrite (since 3.0.0);
- `GETEX key EX|PX` (since 6.0.0) and a conditional `HEXPIRE` (since
  3.0.0), whose deadlines moved at restart;
- a counted `SPOP` over RESP3, which a restart or a replica replayed as a
  different random pop (since 6.3.0);
- stream writes, which a restart or a replica replayed differently from
  how they were answered: an `XADD *` generated new ids, an `XCLAIM` or
  `XAUTOCLAIM` claimed other entries, and a group read made every pending
  entry look just delivered (since 1.4.0; on replicas since 1.18.0);
- a key whose TTL ran out just as the server recorded a relative-TTL
  write, which was removed without an `expired` notification and without
  its deadline frame (since 1.8.1);
- a materialized view declared right after its index over a large
  keyspace, which kept only part of its rows until `VIEW.REBUILD` (since
  3.0.0), and a `DESC` view with `TOPK` built over existing rows, which
  kept a shard's lowest rows instead of its highest (since 3.0.0);
- index entries of rows changed without a command — expiry, eviction, a
  script's call on another key, a replica's full resync — which stayed
  stale until a later write named the row;
- catalog commands run at the same moment on different shards, which
  could drop each other's change after both answered `OK` (since 3.0.0);
- in an embedded store: a key with a TTL, which could still be read for
  up to a reaper tick after its deadline (since 1.11.0); `COPY` of
  anything but a string, which answered `WRONGTYPE` (since 2.0.13); a
  `SET … NX EX`, which set the value and its TTL under two locks, so a
  crash between them left a key that never expired (since 4.0.0); on an
  embedded replica, every stream and geo write, which it dropped, and
  (with more than one shard) most keys, which were written to a shard its
  reads did not look in (since 1.22.0).

The [changelog](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md)
has each one in full.

---

## Getting it

```toml
# Cargo.toml
kevy-embedded = "7.0.0"
```

```sh
npm install @goliapkg/kevy@7.0.0          # wasm
npm install @goliapkg/kevy-node@7.0.0     # Node native
pip install kevy==7.0.0
go get github.com/goliajp/kevy-go/v7@v7.0.0
cargo install kevy --version 7.0.0        # the server binary, from source
npm install -g @goliapkg/kevy-bin@7.0.0   # the server binary, prebuilt
```

Prebuilt server binaries for Linux (x86-64, arm64) and macOS (arm64), each
with a SHA-256 file, are on the
[v7.0.0 release](https://github.com/goliajp/kevy/releases/tag/v7.0.0).
Container users on `ghcr.io/goliajp/kevy:latest` get 7.0.0 on the next
pull; the pinned tag is `ghcr.io/goliajp/kevy:7.0.0`.
