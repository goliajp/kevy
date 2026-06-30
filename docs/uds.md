# Unix-domain socket (UDS) transport

kevy exposes an optional Unix-domain stream listener that speaks identical RESP semantics to the TCP port, letting same-host clients skip the loopback stack entirely.

## When you need this

UDS is the right transport when the client and server share a host:

- **Same-host clients** — application and kevy on one box, or in containers sharing a tmpfs / mounted socket directory.
- **Latency-sensitive workloads** — low connection counts, small payloads, or high-fanout pipelining where the TCP loopback round-trip floor is the binding constraint.
- **Container sidecars** — sidecar + main container sharing a `/run` or `/tmp` volume; the socket file is the IPC handle, no port allocation needed.

Cross-host clients still need TCP — UDS is filesystem-scoped and never leaves the kernel.

## Core idea

Set `KEVY_UNIX_SOCKET` to a filesystem path and kevy dual-binds: the TCP listener stays up exactly as before, and a UDS listener accepts on the same shard runtime with the same RESP2/3 parser. Any RESP client that takes a `unix://` URL or `-s <path>` flag switches over with one line of config. UDS eliminates loopback `rep_movs`, `nft_do_chain`, and the TCP syscall path, so the per-op floor drops materially on every workload.

## Worked example

Start kevy with both transports enabled:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6379
```

Connect via UDS with `redis-cli`:

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
# OK
redis-cli -s /tmp/kevy.sock GET foo
# "bar"
```

TCP on `:6379` is still live in parallel — same data, same shards:

```sh
redis-cli -p 6379 GET foo
# "bar"
```

From Rust, the in-tree client accepts `unix://` URLs:

```rust
let mut conn = kevy_client::Connection::open("unix:///tmp/kevy.sock")?;
conn.set(b"k", b"v")?;
```

## Permissions and security

The trust boundary for UDS is the **filesystem** — there is no RESP-level AUTH or TLS on the Unix socket. Whoever can `open(2)` the socket file can issue any command, including `FLUSHALL`.

- **Socket file ownership.** kevy creates the socket as the user the server runs as. Use `chown` / `chgrp` after start, or run kevy under the identity you want to own the socket.
- **Permission bits.** The socket is created with permissive bits by default so a co-located client process can connect. Tighten by placing the socket inside a directory with restrictive permissions — e.g. `/run/kevy/` owned by a `kevy` group with `0750`, so only group members can `connect(2)`. Directory permissions gate access to the socket inode itself.
- **tmpfs vs disk.** `/tmp` and `/run` on most Linux distros are tmpfs and ideal for sockets (no disk I/O on connect). A persistent path on a real filesystem works too — the inode is just a rendezvous point, no data ever touches the disk.
- **Trust domain.** Treat any account with read+write on the socket path as fully authenticated. If you need per-client identity, that has to live above kevy (a sidecar proxy, a kernel LSM, namespace isolation).

## Server config knobs

| Env var | CLI flag | Default | Effect |
|---|---|---|---|
| `KEVY_UNIX_SOCKET` | (env-only for now) | unset | Filesystem path to bind. Unset = TCP-only. |
| `KEVY_BIND` | `--bind` | `127.0.0.1` | TCP bind address; UDS bind is independent. |
| `--port` | `--port` | `6379` | TCP port; UDS still binds when set. |

Notes:

- **Path must not pre-exist.** kevy refuses to start if `KEVY_UNIX_SOCKET` already points at a file — it will not clobber a path it didn't create. Clean it up on restart (`rm -f /tmp/kevy.sock`) or use a per-run path (`/run/kevy/$(date +%s).sock`). This is intentional: silently unlinking would let a misconfigured kevy steal another service's socket.
- **Dual-bind is always on when the env var is set.** There is no UDS-only mode — the TCP listener stays up too. If you want to forbid TCP, bind it to a loopback-only address you control and firewall it off.
- **Shard 0 owns the accept loop.** Accepted connections are dispatched onto the existing per-shard runtime, so `--threads` still controls parallelism for the workload behind the socket.
- **io_uring path.** On Linux with `KEVY_IO_URING=1`, the UDS accept runs as a multishot accept SQE through the same io_uring instance as TCP — no extra reactor cost. `TCP_NODELAY` is not set on UDS (it isn't an IP socket).

## Trade-offs

UDS vs TCP loopback on the same kevy binary:

| Aspect | UDS | TCP loopback |
|---|---|---|
| Per-op floor | lower (no IP/checksum/port/NAGLE) | higher |
| Reach | same host only | any host |
| Identity | filesystem permissions | port + bind address + AUTH |
| Lifecycle | socket file on disk; must be cleaned on restart | port lifecycle is kernel-managed |
| Observability | `lsof` / `ss -xl` | `ss -tln`, `netstat`, `tcpdump` |
| Client config | `unix:///path` or `-s /path` | `host:port` |

The throughput gain is workload-shape dependent — small-payload low-connection cells gain the most (the loopback per-op tax dominated them); CPU-saturated cells gain less (the transport wasn't the floor). See [bench/REPORT.md](https://github.com/goliajp/kevy/blob/master/bench/REPORT.md) for measured numbers.

## FAQ

### Can I bind UDS and TCP at the same time?

Yes — that's the only mode. Setting `KEVY_UNIX_SOCKET` adds a UDS listener; the TCP listener stays up exactly as it was. Use whichever per-client makes sense.

### Server refuses to start — "socket exists"?

Intentional. kevy will not `unlink` a path it didn't create, because that lets a misconfigured run silently steal another service's socket. Either remove the stale file (`rm -f /tmp/kevy.sock`) before restart, or use a per-run path like `/run/kevy/$(uuidgen).sock`. If kevy crashed and left the file behind, removing it manually is safe.

### How fast is UDS vs TCP loopback?

Materially faster on every workload, because UDS skips the entire IP path: no checksum, no netfilter chain (`nft_do_chain`), no `rep_movs` through loopback, no per-packet ACK round-trip. The exact ratio depends on what fraction of the per-op budget was loopback overhead — single-connection small-payload workloads see the biggest jump; CPU-bound pipelined cells see less. Measure on your workload with `redis-benchmark -s /tmp/kevy.sock` vs `-h 127.0.0.1`.

### Can my client library use UDS?

Most do. `redis-cli` and `redis-benchmark` take `-s <path>`. ioredis, node-redis, redis-py, redis-rb, go-redis, lettuce, jedis, and the in-tree [kevy-client](https://github.com/goliajp/kevy/tree/master/crates/kevy-client) / [kevy-client-async](https://github.com/goliajp/kevy/tree/master/crates/kevy-client-async) all accept `unix:///path` URLs or an explicit socket-path option. Check your driver's connection-options docs for the exact key name.

### Should I drop TCP entirely if all my clients are on the same host?

You can, but you don't have to. Leaving TCP bound to `127.0.0.1` costs nothing if no one connects, and it leaves a fallback if a client's UDS path gets misconfigured. The usual deployment is "UDS for the hot client, TCP for `redis-cli` debugging."
