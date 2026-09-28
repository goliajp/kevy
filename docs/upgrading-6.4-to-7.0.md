# Upgrading from 6.4 to 7.0

The short version: **no code change for a wire client, and the data
directory opens as it is.** The major version is for two Rust-side
changes: `kevy-config`'s section structs gained fields, and kevy-cli's
tools answer only behind `--kevy`, as 6.x announced. What else needs a
look is listed below: two new files an embedded store may leave in its
directory, index sizes that are now reported as they are, and a few
replies that gained fields.

```toml
kevy-embedded = "7.0.0"
```

7.0.0 is about four things: an embedded store that keeps every write a
killed process returned and opens and closes faster; global indexes,
spread over the shards by value; index rows that cost less than half what
they did; and encrypted links — between nodes, and on a second client
port — all off unless configured
([encrypted-links.md](encrypted-links.md)).
It also fixes defects that had been losing data since as far back as 3.0;
they are in [§9](#9-defects-fixed-that-lost-data).

## TL;DR — what to do

| If you… | What changes | § |
|---|---|---|
| run the server, or talk to kevy over the wire | swap the binary; nothing else | — |
| may downgrade to 6.4 | open and close cleanly with 7.0 first; re-declare global indexes | 1 |
| set `MAXMEM` on an index, or size a tiered store near its index floor | index sizes read two to four times the old figure | 2 |
| parse `IDX.LIST` or `IDX.DESCRIBE` positionally | each gains a `partitioning` pair | 3 |
| read an embedded store's change feed or AOF | an `MSET` arrives as one frame per shard | 4 |
| start two servers on one port by accident | the second one now refuses to start | 5 |
| implement `kevy_rt::Commands` | three new methods, all with defaults | 6 |
| build `kevy_config` structs with struct literals | new fields to name, or `..Default::default()` | 7 |
| script `kevy-cli doctor`, `export`, `sql compile` … as bare words | put the tool after `--kevy` | 8 |
| want an index read to reach fewer shards | declare it global | [indexes](indexes.md#global-indexes-partition-global) |

---

## 1. Going back to 6.4: what the directory may hold

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

6.4 knows neither. After a crash or a kill, open the directory once with
7.0 and close it cleanly before going back to 6.4: that drains the ring
and truncates the tail, and leaves files 6.4 reads as it always did.
`Config::with_stage_ring(0)` and `Config::with_mapped_aof(false)` turn the
two off; `AppendFsync::Always` uses neither.

Two more things in the log are new, for a server and an embedded store
alike. A stream group's consumer contacts are recorded as internal
`XINTERNAL.CONSUMERSEEN` frames, which 6.4 skips — it then loses consumers
made only by `XGROUP CREATECONSUMER`. And a catalog with a global index is
written in a sidecar format 6.4 cannot read, so a 6.4 server given it
starts with no indexes, a table's compiled paths included, until they are
declared again.

## 2. Index sizes are reported as they are

The `bytes` of `IDX.LIST`, `IDX.VERIFY` and `TABLE.VERIFY` was `value + key
+ 48` per row, which undercounted the heap. It is now what the rows cost:
about `key + string value + 58…69` bytes per row for a local index — and
the rows are less than half what 6.4's cost, so the figure is larger than
before while the memory is smaller. The same figure feeds an
index's `MAXMEM` and the tiering reservation for indexes, so:

- an index declared with a tight `MAXMEM` can now fail its build with
  `-INDEXOVERBUDGET`;
- a tiered store sized close to its index floor keeps less data hot, or
  refuses a new index by name.

Re-check those budgets against `IDX.LIST` on a loaded sample. The per-row
formula is in [indexes.md](indexes.md#consistency--cost-model).

## 3. `IDX.LIST` and `IDX.DESCRIBE` gain a pair

- Each `IDX.LIST` row ends with `partitioning local|global`; a global
  index adds `partitions`, `max_entries` and `mean_entries`.
- `IDX.DESCRIBE` names the `partitioning` right before `declaration`, which
  stays the last pair.

A client that reads these replies by key sees new keys it can ignore; one
that counts positions does not.

## 4. An embedded `MSET` is one frame per shard

An embedded store used to set each pair of an `MSET` under its own lock
and log it as its own `SET`. It now sets a shard's pairs under one lock
and logs them as one `MSET` frame, so a crash keeps each shard's share
whole or not at all. The AOF, a replica and the change feed see `MSET`
frames where they saw one `SET` per key; a feed consumer that handles only
`SET` should handle `MSET` too, as it already had to for a server.

## 5. A port already held is refused

Every shard listens with `SO_REUSEPORT`, which let a second kevy started by
the same user on the same port join the first one's listeners: both ran,
each took a share of the connections, and writes seemed to vanish from
whichever one a client read back. A port in use now stops startup with
`Address already in use`, as Redis does.

## 6. For Rust embedders of the runtime

`kevy_rt::Commands` gains `take_ext_out`, `apply_ext` and
`extension_targets`, all with defaults that keep 6.4's behaviour. They
carry messages between shards (a write replies only once they are
applied) and let an extension read name the shards it needs. The global
index is built on them.

## 7. `kevy-config`: new fields on the section structs

The encrypted links and the proxy-friendly cluster added settings, and a
setting is a public field:

| struct | new fields |
|---|---|
| `Config` | `secure` (a `SecureSection`: `private_key_file`, `listen_port`, `client_keys`, `cluster_port_base`, `announce_cluster_port_base`) |
| `ClusterSection` | `announce_ip`, `announce_port_base`, `secure`, `peer_keys` |
| `PeerEntry` | `repl_port_base` |
| `ReplicationSection` | `secure`, `upstream_key`, `replica_keys` |

A struct literal that names every field stops compiling. Name the new
fields, or end the literal with `..Default::default()` — every struct
above has a default except `PeerEntry`, whose new field is
`repl_port_base: None` for the old behaviour. A config built by
`Config::load` or `Config::from_toml_str` needs nothing: every new setting defaults to
off.

## 8. kevy-cli: tools answer behind `--kevy` only

kevy-cli 6.4 ran its tools as bare words — `kevy-cli doctor -p 6004`,
`kevy-cli export …`, `kevy-cli sql compile f.sql --apply --url h:p` — and
through 6.x printed a line naming the `--kevy` form. In 7.0 a bare word is
a server command, as it is in redis-cli, so the old line sends `DOCTOR` to
the server and prints its error. Move the tool behind `--kevy`, and the
connection options in front of it:

```sh
kevy-cli -p 6004 --kevy doctor
kevy-cli -p 6004 --kevy sql compile schema.sql --apply   # --url h:p becomes -h h -p p
kevy-cli -p 6379 --kevy diff 127.0.0.1:6380 user:        # the first server is the session's
```

`digest <prefix>`, and `backup`/`restore` in their file shapes, are
`--kevy digest|backup|restore`. The library functions that served the old
forms (`route_tool`, `run_doctor_cli`, `run_shadow_cli`, `run_lint_cli`,
`run_backfill_keys_cli`) are gone with them.

## 9. Defects fixed that lost data

Each of these could lose a write or a deadline without an error:

- a hash field's own TTL, after a background AOF rewrite (since 3.0.0);
- writes made after a process died inside a transaction, at the restart
  after next (since 4.0.0);
- writes made during an AOF rewrite's final swap (since 5.0.0);
- writes made after a `BGSAVE`, on macOS and on Linux without io_uring
  (since 5.1.0);
- `GETEX key EX|PX` and a conditional `HEXPIRE`, whose deadlines moved at
  restart;
- a counted `SPOP` over RESP3, recorded without the members it removed;
- on an embedded replica with more than one shard, most keys, which were
  written to a shard its reads did not look in.

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
