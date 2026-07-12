# Availability

How a kevy deployment stays writable and readable through node failures: the topologies, the consistency ladder from "fastest, asynchronous" up to "quorum-fenced", planned and crash failover, and the exact error contract clients see at every step.

This doc builds on [`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md) (the streaming mechanics) — read that first if you have never brought up a primary/replica pair. If you only run one kevy node, the only section that applies to you is the ladder's first rung.

## Topologies

### Single node

No replication config (`role = "standalone"`, the default). Every write is acknowledged as soon as it lands in the local store; durability is [`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md) (AOF + RDB). There is no failover — availability equals the lifetime of the process.

### Primary + replicas

```toml,fragment
# primary                          # each replica
[replication]                      [replication]
role = "primary"                   role = "replica"
                                   upstream = "primary.internal:16004"
```

The primary streams every applied mutation per shard; replicas apply and serve reads (client writes get `-READONLY`). Two wiring shapes:

- **Fleet** (default): the upstream is a full kevy server — each local shard connects to `(host, port_base + shard_index)`, one stream per shard.
- **`single_source = true`**: the upstream is ONE stream on one port (an embedded writer via `with_embed_writer`) — a single routing runner fans frames into local shards by key hash. See the *Embedded-as-primary* section of [`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md).

Failover in this topology is **manual**: `REPLICAOF NO ONE` on the survivor, retarget the rest.

### Three-node elect quorum

```toml
# every node adds the same [cluster] block
[cluster]
node_id         = "n1"                  # unique per node
elect_port_base = 6204
peers           = "n1@10.0.0.1:6204:6004,n2@10.0.0.2:6204:6004,n3@10.0.0.3:6204:6004"
```

Membership is **static** (the operator-declared `peers` table); roles are **dynamic** (elections move the primary around inside that table). Quorum is `N/2 + 1` — N=2 cannot survive any failure (either node down locks the survivor read-only), so a deployment that needs failover uses N ≥ 3.

Use the extended peer syntax `id@host:elect_port:client_port`: election traffic rides the elect port, while retargeting and `-MISDIRECTED` replies use the client port. With the legacy two-field form the client port is assumed equal to the elect port, which is almost never what you want.

**Election-only write authority.** In an elect quorum, `[replication] role = "primary"` is only an initial *preference*. Every quorum member configured as primary starts **read-only** and holds writes until it wins an election — including on a cold start, where the cluster pays one election round (a few seconds) before the first write is accepted. This unconditional clamp is what prevents the classic "restarted empty primary erases the cluster" accident: a node that crashed and lost its disk can never come back writable on config alone.

## The consistency ladder

Replication is asynchronous by default. Each rung below buys a stronger guarantee for one verb or one config key — pay for exactly what a given read or write needs.

| rung | mechanism | guarantee | cost |
|---|---|---|---|
| 0 | (default) | primary acks after local apply; replicas trail | none |
| 1 | `REPL.TOKEN` + `REPL.WAIT` | read-your-writes on a chosen replica | one blocked call on the replica |
| 2 | `WAIT n timeout` | write is on the primary **and** acked by ≥ n replicas | one blocked call on the primary |
| 3 | `replica_max_staleness_ms` | a replica never serves reads older than the bound (`-STALE`) | reads fail over to the primary during lag spikes |
| 4 | `min_replicas_to_write` | primary refuses writes without n fresh replicas (`-NOREPLICAS`) | write availability coupled to replica health |
| 5 | quorum lease (automatic with elect) | a partitioned primary fences its own writes (`-NOREPLICAS`) | writes pause for the lease window during partitions |

### Read-your-writes: `REPL.TOKEN` / `REPL.WAIT`

```
REPL.TOKEN
REPL.WAIT gen offset [gen offset ...] [TIMEOUT milliseconds]
```

Write to the primary, then `REPL.TOKEN` there: it returns one `(generation, offset)` pair per shard — the live feed tail, which by construction covers your write. Hand the whole token to `REPL.WAIT` on the replica you are about to read: it blocks until every shard has **applied** at least that far, answers `+OK`, and your next read on that connection observes your write. Default `TIMEOUT` is 1000 ms; `0` and anything larger are capped at 60 s.

The `generation` half is the anti-footgun: it identifies one unbroken offset history (the same generation the CDC feed uses — see [`docs/cdc.md`](https://github.com/goliajp/kevy/blob/develop/docs/cdc.md)). A failover, `FLUSHALL`, or crash restart bumps it, so a token minted against the old primary's offset space can never falsely satisfy against the new one — `REPL.WAIT` answers `-MISDIRECTED writer is <primary>` immediately on a generation mismatch, and the client falls back to reading the primary. Timeout gives the same reply: either way, "go read the writer" is the one recovery path.

On a primary, `REPL.WAIT` returns `+OK` immediately (you are already talking to the writer), so the call is safe to issue unconditionally through a routing client.

### `WAIT` — replica acknowledgment, not durability

```
WAIT numreplicas timeout
```

Blocks until **every shard's** `master_repl_offset` (frozen when the barrier arms) has been acknowledged by at least `numreplicas` replicas, or the timeout passes; returns the minimum acked-replica count across shards. The all-shard barrier is deliberate — a kevy write may land on any shard, so per-shard counting is the only answer that is never wrong. `timeout 0` is Redis's "wait forever", hard-capped at 60 s. Issued on a replica, `WAIT` answers `-ERR WAIT cannot be used with replica instances`.

**`WAIT` is not durability.** A replica ACK means the frame reached the replica's apply pipeline, not that any fsync happened anywhere. What `WAIT 1` does buy: the write exists on two nodes, so it survives any single-node loss *provided the subsequent election picks the most-advanced candidate* — which it does (see crash failover below). For power-loss durability, pair replication with AOF on the primary.

### Bounded staleness: `-STALE`

```toml
[replication]
replica_max_staleness_ms = 2500     # 0 = off (default)
```

A replica whose last primary heartbeat is older than the bound refuses reads with `-STALE replica is stale; read the primary or raise replica_max_staleness_ms`. Heartbeats ride the replication stream at 1 Hz, so bounds below ~2 s will trip on healthy links; the gate uses 2500 ms. The replica recovers the moment heartbeats resume — no operator action.

### Write gates: `min_replicas_to_write` and the quorum lease

```toml
[replication]
min_replicas_to_write = 1           # 0 = off (default)
```

A heuristic modeled on Redis: the primary refuses writes with `-NOREPLICAS Not enough good replicas to write.` when fewer than N replicas are healthy. Healthy means a live replication connection that has ACKed **and** whose latest ACK is younger than `min_replicas_max_lag_ms` (default 10 000 ms) — a stalled replica that stops ACKing ages out of the count even while its TCP connection stays up. This closes the "primary writing into the void" window but is **not** a split-brain guarantee — both sides of a partition can each see their own replicas.

The real fence is the **quorum lease**, automatic in an elect quorum: a primary whose election heartbeats cannot reach a strict majority of peers within the lease window (= `down_after`, 5 s) fences its own writes with `-NOREPLICAS primary lost quorum; writes fenced`, and unfences when the partition heals. Combined with `WAIT` or tokens this squeezes "the minority side silently absorbed writes" down to at most one lease window, and every write inside that window fails *loudly* rather than silently diverging.

## Failover

### Planned: the `FAILOVER` verb

```
FAILOVER host port [TIMEOUT ms]      # host:port = the target replica's CLIENT address
FAILOVER ABORT
```

Run on the primary; answers `+OK` at once and runs the handover on a background thread (asynchronous, like Redis's `FAILOVER`):

1. **Quiesce** — every new client write answers `-QUIESCED migrating to <host:port>`; the [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) client already retries these with backoff, so writers stall rather than fail.
2. **Drain** — the old primary polls the target's `INFO replication` until `master_link_status:up` and `slave_lag_frames:0`. With writes quiesced, converged gauges are exact — this is the zero-loss step.
3. **Promote + follow** — `REPLICAOF NO ONE` is sent to the target (its feed generation bumps, fencing stale tokens), then the old primary retargets itself at the target's replication port and becomes the read-only replica.
4. Un-quiesce. Stray writes still aimed at the old node now get `-READONLY` and reroute.

`FAILOVER ABORT` clears the quiesce at any point before promotion; the background thread notices and stands down. If the target never drains within `TIMEOUT` (default 10 000 ms), the quiesce rolls back and the node resumes primary duty — the failed attempt costs one write-availability blip and nothing else.

One addressing constraint: the handover retargets to `client port + 10000`, so the target must be running with the default `listen_port_base` (see port conventions below).

### Crash: quorum election

With the `[cluster]` block configured on every node, a dead primary is detected and replaced without an operator:

1. Peers miss its election heartbeats for `down_after` (5 s) and flag it DOWN.
2. An eligible replica starts a candidacy: it must hold the **highest replication offset** (sum of applied stream positions across shards) among alive peers, with lowest `node_id` as the tie-breaker. Offset-best ordering is what turns `WAIT`-acked writes into survivors — the replica that has your acked write outranks the ones that don't.
3. The candidate needs quorum `ACCEPT`s within `election_timeout` (3 s); the epoch and vote are persisted to `<data_dir>/elect.meta` *before* they leave the node, so a crash-restart can never double-vote.
4. The winner broadcasts `ANNOUNCE`, stops its runner fleet (writes open), and bumps its feed generation. Losers retarget their replication upstream at the winner automatically.

**MTTR ≈ `down_after` + one election round** — about 5–8 s with the shipped timings; the gate bounds it at 30 s end-to-end including write reopening. The election timings (`hb_interval` 200 ms, `down_after` 5 s, `election_timeout` 3 s) are fixed constants in this release, not config keys.

**The old primary comes back.** The restart-role clamp starts it read-only (election-only write authority, above). The election tells it who the current primary is; it retargets and handshakes. Any writes it absorbed after the partition but before dying form a *forked suffix* — its stream position is ahead of the new primary's, and the new primary answers the only safe way: **discard the fork**, ship a full snapshot, resume live frames. The rejoined node converges on the majority's history; the forked writes are gone (they were never `WAIT`-acked by definition — the fork is exactly the un-replicated tail).

Failed nodes are **not** auto-replaced and membership never changes at runtime — replacing hardware means updating `peers` on every node and restarting them. Dynamic membership, multi-primary, and cross-DC are out of charter.

## Error contract for writers and readers

The full catalog lives in [`docs/error-replies.md`](https://github.com/goliajp/kevy/blob/develop/docs/error-replies.md); this table is the availability slice — what a client sees at each point of the topology's lifecycle, and what it should do. `kevy-cluster-rw::ReadWriteClient` implements all of these behaviors already (writes to the primary, reads round-robined across replicas, redirect-following on `MISDIRECTED`/`QUIESCED`, a consistent-read path that forces the primary).

| reply | you are talking to | when | client action |
|---|---|---|---|
| `-READONLY You can't write against a read only replica.` | a replica (incl. a demoted or clamped ex-primary) | any client write while `replica_read_only = true` (default) | send writes to the primary; a routing client re-resolves it |
| `-QUIESCED migrating to <host:port>` | a primary mid-`FAILOVER` | the quiesce window (handover step 1–3) | retry with backoff; follow `<host:port>` once the handover lands |
| `-MISDIRECTED writer is <host:port>` | a replica (`REPL.WAIT`), or a non-owner node (scoped writes) | read-your-writes not servable (timeout / generation mismatch), or a scope-routed write | read (or write) at `<host:port>` — the current writer |
| `-NOREPLICAS Not enough good replicas to write.` | a primary with `min_replicas_to_write` | fewer than N healthy replicas | back off and retry; page whoever runs the replicas |
| `-NOREPLICAS primary lost quorum; writes fenced` | a quorum primary on a partition's minority side | quorum lease lost | back off and retry — either the partition heals or the majority elects a new primary your routing client will find |
| `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` | a replica with a staleness bound | primary heartbeat older than the bound | read the primary until the replica catches up |
| `-LOADING kevy is loading the dataset in memory` | a replica mid-full-resync | a snapshot ship is replacing the replica's keyspace wholesale | wait and retry — the window is bounded by the ship; `PING` / `INFO` / `HELLO` still answer, so health checks keep passing |

Two rules of thumb: every one of these is **retryable by design** (nothing was applied), and every one names the topology truth a routing client needs to self-heal — none requires a human in the loop.

## Operations

### Port conventions

| plane | port | notes |
|---|---|---|
| client RESP | `server.port` (e.g. 6004) | the address clients and `peers` client-ports refer to |
| replication | `listen_port_base + shard_i`; default base = client port + 10000 | `nshards` consecutive ports; since v3.15 **replicas bind this listener too** (promotion symmetry) |
| election | `elect_port_base`; default = client port + 200 | one control-plane listener per node |

Both automatic retarget (election) and `FAILOVER` assume the `client port + 10000` replication convention — leave `listen_port_base` at its default in any failover-enabled deployment. Running several instances on one host: keep client ports at least `nshards` apart, or the instances' replication port ranges collide.

### Config keys

`[replication]` (see [`crates/kevy-config/src/replication.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/replication.rs)):

| key | default | meaning |
|---|---|---|
| `role` | `"standalone"` | `"primary"` streams to replicas; `"replica"` pulls from `upstream`; standalone = subsystem dormant |
| `upstream` | unset | replica-only: `host:port` of the primary's replication port base |
| `listen_port_base` | `0` (= client port + 10000) | shard `i` binds replication at base + `i` |
| `replication_buffer_size` | `256mb` | per-shard ring backlog; reconnects inside it skip the snapshot |
| `reconnect_window_ms` | `60000` | how long a disconnected replica's slot is held |
| `replica_read_only` | `true` | reject client writes on a replica (`-READONLY`); `CONFIG SET` is the escape hatch |
| `replica_max_staleness_ms` | `0` (off) | ladder rung 3: `-STALE` reads past the bound |
| `min_replicas_to_write` | `0` (off) | ladder rung 4: `-NOREPLICAS` under N healthy replicas |
| `min_replicas_max_lag_ms` | `10000` | rung 4's freshness window: a replica counts as healthy only if its latest ACK is younger than this bound — a stalled replica no longer satisfies `min_replicas_to_write` |
| `single_source` | `false` | one-stream upstream (embedded writer) instead of the per-shard fleet |

`[cluster]` election keys (see [`crates/kevy-config/src/cluster.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/cluster.rs)): `node_id` (≤ 32 B ASCII, unique), `elect_port_base`, and `peers` (`id@host:elect_port[:client_port],...`). The elector stays dormant unless both `node_id` and `peers` are set.

### Observability

`INFO replication` on a **replica**:

| field | truth source |
|---|---|
| `role:slave` + `master_host` / `master_port` | the live upstream (a runtime `REPLICAOF` wins over config) |
| `master_link_status` | `up` if a stream heartbeat landed within 3 s, else `down` |
| `master_last_io_seconds_ago` | age of the last heartbeat |
| `slave_read_only` | the `-READONLY` gate |
| `slave_repl_offset` | applied stream position |
| `slave_lag_frames` | primary's announced tail minus applied — **0 means caught up** |

`INFO replication` on a **primary**:

| field | truth source |
|---|---|
| `role:master`, `master_repl_offset` | stream tails **summed across all shards** (the same convention the election's offset-best ordering uses) |
| `connected_slaves` | distinct replica processes with live connections (a replica's per-shard streams fold into one entry) |
| `slaveN:ip=…,port=…,state=…,offset=…,sent=…,lag=…` | per replica process, identified by its peer address: `state` is `online` once **every** shard stream has ACKed (`syncing` while any hasn't), `offset` is its acked position summed across shards, `lag` in frames |

The cross-process comparables are the replica's own `slave_lag_frames` gauge and the data itself — offset arithmetic between two different processes' INFO outputs is only meaningful within one generation.

`ROLE` gives the same truths in Redis's array shape (`master` + offset + per-replica `[ip, port, acked-offset]`, or `slave` + upstream); with an elect quorum configured, the live election role wins over both `REPLICAOF` state and config. Watching lag in practice: poll `slave_lag_frames` on replicas (alert on non-zero sustained past your staleness budget), and `WAIT 1 <small-timeout>` on the primary as a cheap end-to-end "is at least one replica keeping up" probe.

### The gate

Everything this doc promises is executable: [`bench/availgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/availgate.sh) runs 13 clamps against real processes — phase 1 (READONLY-while-applying, offset/lag truth, link-down/up detection, per-replica ack truth, min-replicas), phase 2 (3-node crash failover with MTTR bound, restart-role clamp + fork-discard rejoin), phase 3 (WAIT truth, 20/20 read-your-writes rounds + future-token MISDIRECT, `-STALE` on a SIGSTOP'd primary, quorum-lease fence and reopen), phase 4 (the `-LOADING` contract across a full-resync ship window, `PING` exempt). If a claim here and the gate ever disagree, the gate wins — file the doc bug.

## See also

- [`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md) — stream mechanics, snapshot ship, embed-as-replica, sizing the backlog.
- [`docs/cdc.md`](https://github.com/goliajp/kevy/blob/develop/docs/cdc.md) — the `(generation, offset)` cursor model that `REPL.TOKEN` shares.
- [`docs/error-replies.md`](https://github.com/goliajp/kevy/blob/develop/docs/error-replies.md) — the full error catalog.
- [`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md) — the durability half of the story (`WAIT` is not it).
- [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md) — the election wire protocol.
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — the client that implements the whole error contract.
