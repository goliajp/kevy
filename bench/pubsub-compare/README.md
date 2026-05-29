# 6-way pub/sub bench

Reproducible comparison of `kevy` vs the 5 messaging systems most often
proposed as alternatives:

| System | Mode | Protocol |
|---|---|---|
| **kevy** | broker (RESP) | TCP loopback |
| valkey 9.1 | broker (RESP) | TCP loopback |
| redis 7.4 | broker (RESP) | TCP loopback |
| ZeroMQ 4.3.5 | direct messaging | `tcp://127.0.0.1:5556` |
| Zenoh 1.9 | peer (mesh) | local TCP/UDP |
| Aeron 1.45 | direct messaging | IPC (shared memory) |

Each run drives the same load shape: **1 publisher → N=50 subscribers,
M=200 000 publishes, 16-byte payload**, isolated cores
(server 0-9 / loadgen 10-15), 2 measurement runs per backend.

The reported metric is `delivered msg/s = M × N / wall-clock`, i.e. the
total fan-out the system sustains.

## Why these six?

- **kevy / valkey / redis**: the Redis-protocol cohort — same wire
  format, same API, three different implementations of the same
  semantics.
- **ZeroMQ**: the most widely-deployed "low-level messaging library"
  reference. No broker, no protocol overhead. Long the default
  upper-bound comparison for any pub/sub work.
- **Zenoh**: modern Rust pub/sub-and-query, designed by Eclipse for
  IoT / data-fabric workloads. Compares native-Rust-with-discovery.
- **Aeron**: Real Logic's UDP- and shared-memory-based messaging,
  industry-standard for low-latency finance & telemetry. Aeron IPC
  is the absolute hardware ceiling — no kernel network stack
  involved. Other TCP-based systems can't reach it structurally; it
  serves as the "what if there was no protocol at all" reference.

## Running

```bash
cd bench/pubsub-compare
docker compose --profile build build   # ~10 min first time
bash run.sh                            # ~2-3 min
```

Override per-test parameters via env:

```bash
SUBS=100 MSGS=500000 SIZE=64 bash run.sh
```

## Latest results (16-core Linux box, 2026-05-28)

`SUBS=50 MSGS=200000 SIZE=16`:

| System | Delivered msg/s | vs valkey-default |
|---|---:|---:|
| Aeron 1.45 IPC | **26.5 M** | 3.90× |
| kevy epoll | **18.2 M** | 2.68× |
| kevy io_uring | 17.8 M | 2.62× |
| ZeroMQ 4.3.5 | 9.3 M | 1.37× |
| redis 7.4 | 8.5 M | 1.25× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.7 M | 0.40× |

Key takeaways:

- **Aeron IPC is the absolute upper bound** (shared memory, no kernel
  network stack). Any TCP-based system is structurally below this.
- **kevy is the fastest TCP / RESP-protocol implementation**, 2× faster
  than ZeroMQ on the same TCP loopback transport despite RESP overhead.
- **kevy beats valkey 9.1 by 2.7×** at this fan-out workload (lift over
  the earlier 2.3× baseline).
- Zenoh underperforms here because 50-way fan-out hits its tokio
  channel path; Zenoh's strength is query/put semantics, not broadcast.

## Source files

- [`Dockerfile.kevy`](Dockerfile.kevy) — kevy release server image.
- [`Dockerfile.loadgen`](Dockerfile.loadgen) — one image with every
  pub/sub bench binary (kevy-pubsub-bench, zmq_pubsub_bench, zenoh_pubsub,
  aeron_pubsub, redis-cli).
- [`zmq_pubsub_bench.c`](zmq_pubsub_bench.c) — C, libzmq 4.3.5.
- [`zenoh_bench/`](zenoh_bench/) — Rust, zenoh 1.9.
- [`aeron_bench/`](aeron_bench/) — Rust, rusteron-client 0.1 (embeds
  Aeron 1.45 media driver).
- [`run.sh`](run.sh) — orchestrator (bring up each server, run client,
  tear down).

## Dependency note

The bench source pulls in `zenoh`, `tokio`, `rusteron-client` /
`rusteron-media-driver`, and links against libzmq. These are
**benchmark-only** dependencies, isolated under `bench/pubsub-compare/`
— the kevy product crates remain zero-crates.io.
