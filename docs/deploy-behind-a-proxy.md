# Deploying kevy behind a proxy

kevy has no AUTH and no TLS, and will not get them — that is a charter
decision, not a gap waiting to be filled. Everything that authenticates
or encrypts happens **in front of** the process. This chapter is the
recipe for that, and it is meant to be copied rather than adapted.

## What kevy exposes

| | Ports | Notes |
|---|---|---|
| Default | **one** (`6004`) | `--threads N` opens N listeners on that *same* port via `SO_REUSEPORT`, not N ports |
| `--cluster` | **1 + N** | main port, plus `port+1+i` per shard |
| `KEVY_UNIX_SOCKET=<path>` | unchanged | a unix socket is **added**; the TCP listener stays up |

The default bind is `127.0.0.1`. That is already the shape this chapter
wants: the engine listens where only this host can reach it, and the
only thing with a public address is the terminator.

## The shape

```
   client ──TLS──▶  terminator  ──plain──▶  kevy
  (rediss://)     (stunnel / HAProxy /     127.0.0.1:6004
                   nginx stream)           or /run/kevy/kevy.sock
```

Nothing about kevy changes. RESP carries no host name, no SNI, no
absolute URLs — a byte proxy in front of it is invisible to both sides.

## RESP is not HTTP

An HTTP reverse proxy cannot carry RESP, and that includes **stock
Caddy**: its core has no layer-4 module, so `caddy` alone cannot put TLS
in front of kevy no matter how the Caddyfile is written. You need a
TCP-level terminator:

- **stunnel** — smallest, packaged everywhere, does exactly this one job;
- **HAProxy** in `mode tcp` — reach for it if you already run HAProxy or
  want health checks and failover in the same place;
- **nginx** with the `stream` module — same, if nginx is already there.

### stunnel → loopback port

```ini
[kevy]
accept  = 0.0.0.0:6379
connect = 127.0.0.1:6004
cert    = /etc/kevy/tls/fullchain.pem
key     = /etc/kevy/tls/privkey.pem
```

### HAProxy → unix socket

Tighter: the engine's socket file has filesystem permissions, so the
terminator is the only process that can reach it even from this host.

```
listen kevy
    bind :6379 ssl crt /etc/kevy/tls/kevy.pem
    mode tcp
    timeout client 0
    timeout server 0
    server kevy unix@/run/kevy/kevy.sock
```

Start kevy with the socket:

```console
KEVY_UNIX_SOCKET=/run/kevy/kevy.sock kevy --dir /var/lib/kevy
```

Two things about that path. kevy **refuses to start** if it already
exists — it will not clobber a path it did not create — so clean it up
on restart or use a per-run path. And the TCP listener on `127.0.0.1`
stays up regardless; the socket is an addition, not a replacement.

### nginx stream → unix socket

```nginx
stream {
    upstream kevy { server unix:/run/kevy/kevy.sock; }
    server {
        listen 6379 ssl;
        ssl_certificate     /etc/kevy/tls/fullchain.pem;
        ssl_certificate_key /etc/kevy/tls/privkey.pem;
        proxy_pass kevy;
        proxy_timeout 1h;
    }
}
```

Note the timeouts in all three: a blocking `BLPOP` or an idle Pub/Sub
subscriber holds a connection open with no bytes on it for as long as
the application wants. A proxy that reaps idle connections will look
exactly like kevy dropping subscribers.

## The client side

Any stock Redis client with TLS enabled — `rediss://host:6379` — talks
to a terminated kevy unchanged.

**`kevy-cli` cannot.** It rejects `rediss://` with `Unsupported`,
because kevy ships without TLS and the CLI has no TLS stack to lend it.
That is an operational consequence worth planning for rather than
discovering: administer from the host itself, or over an SSH tunnel:

```console
ssh -N -L 6004:127.0.0.1:6004 you@host   # then: kevy-cli -p 6004
```

## Only expose what is necessary

With the default bind, **kevy needs no firewall rule at all** — it is
not reachable off-host. The terminator's port is the only one to open.
If you must bind kevy to a real interface, then the firewall is doing
the job the loopback bind was doing for free, and it is the only thing
between the network and an unauthenticated database.

## Cluster mode does not survive this

Single-node cluster mode is for clients **on the same host**, and a
proxy does not extend it. `CLUSTER SLOTS` and `CLUSTER NODES` advertise
the address kevy is bound to, substituting `127.0.0.1` for a `0.0.0.0`
wildcard (an unroutable advertise would strand every client). There is
no announce-address knob, so a key-aware client that is told
`127.0.0.1:6101` cannot follow that from anywhere else.

If you need remote key-aware routing, bind the routable address and map
those `1 + N` ports through one-to-one — which is the opposite of
exposing a single port. Pick one; they do not compose.

## What was verified, and what was not

Measured against this tree while writing this chapter:

- the port surface in the table above, including `--cluster` opening
  `port+1+i` per shard;
- a plain TCP proxy in front of kevy is transparent (`PING`, `SET`,
  `GET` through a forwarder);
- **TLS termination round-trips RESP against an unmodified kevy** — over
  a loopback port and over a unix socket, TLS 1.3, `+PONG` / `+OK` /
  the stored value back;
- stock Caddy (2.11.4) ships no layer-4 module;
- `CLUSTER SLOTS` advertising `127.0.0.1` under a `0.0.0.0` bind;
- `kevy-cli` rejecting `rediss://`.

The three config blocks are each product's standard form for this job;
they were not run here. What was verified is the shape they implement.

## See also

- [uds.md](uds.md) — the unix socket in detail
- [cluster.md](cluster.md) — what single-node cluster mode is for
- [tuning.md](tuning.md) — `--threads`, and why fewer can be faster
