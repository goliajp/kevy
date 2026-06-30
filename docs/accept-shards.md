# `--accept-shards N` — fold sparse connections onto a subset of shards

`--accept-shards N` tells kevy to bind its listeners and arm `accept` only on shards `0..N`, leaving the rest as compute-only workers that still serve cross-shard reads and writes.

## When you need this

Each kevy shard runs its own reactor with a busy-poll body. That body amortizes its per-iteration overhead (waker drain, inbox check, accept arm) only when each iteration sees enough productive events. If `concurrent_conns / threads` drops below roughly one, every shard spends most of its iterations idle, and aggregate throughput falls below what a 1- or 2-thread server would achieve on the same machine.

Reach for `--accept-shards N` when:

- The client count is small and known (typical app servers, replication followers, internal services).
- You still want `--threads` to match the core count for keyspace parallelism on cross-shard hops.
- Benchmarks show that adding shards makes throughput worse, not better.

Leave it unset when the conn count is high (≥ 1 conn per shard on average), unpredictable, or driven by a thundering-herd client pool — the default of "every shard accepts" is the right answer there.

## Core idea

A static accept-set is declared at boot. Shards inside the set bind the listener fd and arm an `accept` SQE; shards outside the set do neither, so the kernel's `SO_REUSEPORT` group never routes a SYN to them. Off-accept-set shards still own their keyspace slice and still drain the cross-shard inbox, so writes that hash to their slice are dispatched to them, executed, and the reply travels back through the owning conn's shard. The stateless-shard model is intact — every shard runs the same code path; some just happen to own zero connections.

```
                      clients (SO_REUSEPORT group)
                                 |
                  +--------------+--------------+
                  |              |              |
              shard 0        shard 1        shard 2          <-- accept-set (--accept-shards 3)
              listen + accept + own conns + own keyspace slice
                  |              |              |
                  +------ cross-shard dispatch -------+
                  |       |       |       |       |       |
              shard 3  shard 4  shard 5  shard 6  shard 7  shard 8  shard 9   <-- compute-only
              no listener, no accept; execute dispatched cmds, own keyspace slice
```

## Worked example

Ten threads, three accept shards, listening on the standard Redis port:

```sh
kevy --threads 10 --accept-shards 3 --port 6379
```

Equivalent environment-variable form:

```sh
KEVY_THREADS=10 KEVY_ACCEPT_SHARDS=3 kevy --port 6379
```

Equivalent TOML (`kevy.toml`):

```toml
[server]
threads = 10
accept_shards = 3
port = 6379
```

Precedence is CLI > env > TOML > default. `accept_shards` must satisfy `1 <= accept_shards <= threads`; anything outside that range fails fast at startup with exit code 2. The default is unset, which means every shard accepts and the binary behaves byte-identically to a build without the flag.

## Sizing heuristic

Rule of thumb: `accept_shards ≈ ceil(conns / 20)`. The empirical sweet spot is the plateau where each accept shard owns roughly 15–25 connections — dense enough for the busy-poll body to amortize, sparse enough to keep cross-shard dispatch from saturating the inbox channel.

| concurrent conns | threads | recommended `--accept-shards` | conns / accept shard |
|-----------------:|--------:|------------------------------:|---------------------:|
| 10               | 10      | 1                             | 10                   |
| 20               | 10      | 1                             | 20                   |
| 50               | 10      | 2 or 3                        | 25 or 17             |
| 100              | 10      | 5                             | 20                   |
| 200              | 10      | 10 (default, unset)           | 20                   |
| 50               | 16      | 2 or 3                        | 25 or 17             |
| 100              | 16      | 5 or 6                        | 20 or 17             |
| 500              | 16      | 16 (default, unset)           | 31                   |

For the canonical `--threads 10 -c 50 -d 65536 SET` workload, `--accept-shards 3` lifts throughput +10.6% over the default by collapsing 50 conns from ~5/shard onto ~17/shard. The same plateau holds at `--accept-shards 2` (25 conns/shard).

## Trade-offs

| dimension                       | accept-set shard                          | compute-only shard                                  |
|---------------------------------|-------------------------------------------|-----------------------------------------------------|
| busy-poll amortization          | high (own conn fan-in drives per-iter work) | depends on cross-shard inbox rate                  |
| per-conn read / write cost      | local, no hop                             | n/a (no owned conns)                                |
| cross-shard dispatch cost       | one channel send per hop                  | one channel recv + execute + reply send             |
| keyspace ownership              | own slice                                 | own slice                                           |
| CPU floor at idle               | accept-armed, blocks in `io_uring_enter`  | spins to `URING_SPIN_LIMIT` then blocks             |
| effect on tail latency          | accept SQE adds work to hot loop          | cleaner hot loop; cross-shard hop adds one channel  |

Concretely, the smaller `N` you pick, the more each conn benefits from a hot accept-set shard, but the higher the fraction of writes that take a cross-shard hop (`(threads - N) / threads` of all keys). The plateau at conns/shard ≈ 15–25 is where the busy-poll win pays for the extra hops; well below it the dispatch cost dominates, well above it you're back to the unset default.

## FAQ

**Should I always set `--accept-shards`?**
No. If `concurrent_conns / threads >= 1` on average, the default (every shard accepts) is already optimal — the busy-poll body amortizes without help. `--accept-shards` is for the sparse-conn regime where adding shards hurts throughput.

**How do I pick `N` for my workload?**
Start from the rule of thumb `ceil(conns / 20)`. Sweep `N` in a perfgate across `{ceil(conns/25), ceil(conns/20), ceil(conns/15)}` and pick the one that maximises throughput at your target tail-latency budget. Anywhere on the 15–25 conns/shard plateau is a safe production setting.

**Does it break replication, AOF, or persistence?**
No. Off-accept-set shards run the same reactor loop, the same TTL reaper, the same AOF writer, and the same replication ticks as accept-set shards. The only thing they skip is binding the listener and arming `accept`. Replication followers and snapshot/AOF restore behave identically regardless of `N`.

**How do off-accept-set shards still serve writes?**
They own their slice of the keyspace. When a client connection on an accept-set shard issues a write whose key hashes to a compute-only shard, the accept-set shard pushes an `Inbound::RequestBatch` onto the owning shard's inbox; the owning shard executes the command against its own keyspace slice and sends the reply back through the originating shard's reply channel. From the client's perspective the hop is invisible — it sees one reply on one connection, in order.

**What if I get the number wrong?**
The cost of a too-small `N` is a saturated cross-shard inbox and worse tail latency; the cost of a too-large `N` is the original sparse-conn inversion. Both are recoverable by restart with a new value — there's no on-disk state tied to the choice.
