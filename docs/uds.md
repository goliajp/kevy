# Unix-domain socket (UDS) transport

v1.25 added an opt-in Unix-domain stream socket listener — kevy's
peer to valkey/redis's `unixsocket` config. For local clients (same
host, same trust boundary as the server process), UDS skips the TCP
loopback stack entirely: no IP header, no checksum, no port lookup,
no NAGLE/ACK ping-pong. The wire is RESP2 / 3 byte-for-byte, so any
existing client switches over with one URL change.

## When to use it

UDS is the right transport when **all three** are true:

1. The client is on the **same host** as the server (containers
   sharing a tmpfs/host volume count).
2. You're CPU-bound on per-syscall network overhead — small payloads,
   high connection count, single-shard server, or `-c1` workloads
   that pay the full per-op RTT.
3. The trust domain is **the host filesystem** (UDS permissions are
   filesystem permissions; no AUTH/TLS on either kevy or valkey).

UDS is **not** a substitute for TCP when:

- The client lives in a different container *without* a shared
  socket mount (the `/tmp/kevy.sock` path has to be visible to both).
- You need network reachability — TCP loopback is the only choice for
  remote clients (kevy is single-DC, no public-internet design).
- The workload is `-c50 -P16` pipelined and already saturates the
  server's CPU — UDS shaves a few percent there but the lever isn't
  the transport.

For Phase A decomposition / why kevy's UDS gains are bigger than
valkey's, see [`bench/REPORT.md`](../bench/REPORT.md).

## Server setup

Set `KEVY_UNIX_SOCKET` to a filesystem path before launch. The
server **binds the UDS in addition to the TCP listener** — both
accept connections in parallel; choose per-client which one to use:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
```

Behaviour:

- The path is `unlink`ed before bind (stale socket from a previous
  crash is cleaned up automatically — mirrors valkey/redis).
- The socket is `chmod 0777` after bind (any local user can connect;
  tighten with a containing directory's permissions if you need
  per-user access control).
- Only **shard 0** owns the UDS listener; accepted connections are
  dispatched onto the existing per-shard runtime, so the
  `--threads` setting still controls parallelism for the workload
  behind the socket.
- On Linux with `KEVY_IO_URING=1`, the UDS accept loop runs as a
  multishot accept SQE through the same io_uring instance as TCP —
  no extra reactor cost. TCP_NODELAY is skipped for UDS (it's not
  an IP socket).
- Empty / unset `KEVY_UNIX_SOCKET` = TCP-only (v1.24 and earlier
  behaviour unchanged).

The CLI / TOML equivalent is planned for v1.26; for now the env var
is the single knob.

## Client setup

Every Redis/RESP client that takes a Unix-socket option works
out-of-the-box — same RESP2/3 framing.

`redis-cli` / `redis-benchmark` (the `-s` flag):

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
redis-cli -s /tmp/kevy.sock GET foo
redis-benchmark -s /tmp/kevy.sock -t set,get -n 100000 -c 50 -P 16
```

[`kevy-client`](../crates/kevy-client) and
[`kevy-client-async`](../crates/kevy-client-async) accept `unix://`
URLs:

```rust
let mut conn = kevy_client::Connection::open("unix:///tmp/kevy.sock")?;
conn.set(b"k", b"v")?;
```

valkey / redis comparison (their `unixsocket` directive):

```sh
valkey-server --unixsocket /tmp/valkey.sock --unixsocketperm 777 \
              --io-threads 10
redis-server  --unixsocket /tmp/redis.sock  --unixsocketperm 777
```

## Benchmark numbers

Precision bench, n=1 M × 10 runs, 2σ-filtered mean, CI95 < 1 % across
all cells. lx64, `mitigations=off`, kevy `--threads 1` (single shard),
valkey `--io-threads 10`. Reproduce with
[`bench/v125-precision-uds.sh`](../bench/v125-precision-uds.sh).

| workload | kevy 1.25 (UDS) | valkey 9.1 (UDS) | kevy / valkey |
|----------|----------------:|-----------------:|--------------:|
| -c1 SET | **166 k/s** | 96 k/s | **1.73×** |
| -c1 GET | **168 k/s** | 106 k/s | **1.59×** |
| -c50 -P1 SET | 339 k/s | 334 k/s | tied (per-syscall floor) |
| -c50 -P1 GET | 337 k/s | 332 k/s | tied (per-syscall floor) |
| **-c50 -P16 SET** | **4.11 M/s** | 1.75 M/s | **2.35×** |
| **-c50 -P16 GET** | **4.35 M/s** | 3.42 M/s | **1.27×** |
| -c100 -P1 SET | 331 k/s | 326 k/s | tied |
| -c100 -P1 GET | 335 k/s | 327 k/s | tied (1.02×) |

UDS vs TCP for kevy (same server, same benches), how much each
workload gains by switching transport:

| workload | TCP rps | UDS rps | UDS / TCP |
|----------|--------:|--------:|----------:|
| -c1 SET | 94.7 k | 166 k | **1.76×** |
| -c1 GET | 97.3 k | 168 k | **1.73×** |
| -c50 -P1 | 192 k | 339 k | **1.77×** |
| -c50 -P16 SET | 2.59 M | 4.11 M | **1.59×** |
| -c50 -P16 GET | 2.67 M | 4.35 M | **1.63×** |

Why kevy's UDS gain is larger than valkey's: valkey's hot path is
more CPU-bound (per-op work in `processCommand` / `addReply`),
so its TCP ceiling sits below the transport's RTT floor — removing
loopback doesn't hand valkey as much headroom. kevy's hot path is
already light enough that the TCP RTT floor was the binding
constraint at `-c50 -P16`; UDS lifts the constraint and the server
runs out faster than the load generator can drive it. The c=50/100
-P1 ties stay tied even on UDS — both servers saturate the
per-syscall round-trip floor (~3 µs × 50 conns), not anything
transport-specific.

## Security caveats

- **Filesystem permission = AUTH equivalent.** UDS has no native
  authentication; whoever can `open(2)` the socket file can issue
  any command (including `FLUSHALL`). The kevy default `chmod 0777`
  matches valkey/redis defaults; tighten it by putting the socket
  inside a directory with restrictive permissions, e.g.
  `/run/kevy/kevy.sock` owned by the `kevy` group.
- **Stale socket on crash.** kevy `unlink`s before bind, so a stale
  file from a previous crash doesn't block startup. If two kevy
  instances point at the same path, the second wins — the first's
  clients then get `EPIPE` on next write.
- **No remote use.** UDS is host-local. Cross-host clients must use
  TCP (and kevy stays single-DC, no AUTH/TLS — see
  [`README.md`](../README.md#when-to-use-kevy)).

## Reproduce

```sh
ssh lx64
bash /path/to/kevy/bench/v125-precision-uds.sh
```

The precision harness builds the same kevy binary, brings up kevy
and valkey one at a time, runs `redis-benchmark -s <sock>` 10×
each at n=1 M, and prints filtered means with CI95. The
companion smoke test
[`bench/v125-uds-smoke.sh`](../bench/v125-uds-smoke.sh) (14 test
groups, 39 assertions covering SET/GET, every collection,
INCR/APPEND, large values, SETEX, MSET, pipelined DBSIZE, pub/sub,
FLUSHALL, INFO) confirms UDS is wire-equivalent to TCP — same code
path on the server, only the accept SQE differs.
