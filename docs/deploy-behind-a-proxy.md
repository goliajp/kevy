# Deploying kevy behind a proxy

kevy has no AUTH and no TLS. Everything that authenticates or encrypts
happens **in front of** the process. This chapter is the recipe for
that, and it is meant to be copied rather than adapted. (Rust clients can
instead use kevy's own encrypted client port over `kevys://`; see
[encrypted-links.md](encrypted-links.md).)

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
   + client cert   nginx stream)           or /run/kevy/kevy.sock
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

Every block below both encrypts **and requires a client certificate**
signed by your CA. Leave the client-certificate lines out and the proxy
encrypts traffic for anyone who connects, which for a database with no
AUTH means anyone can still read and write every key.

### stunnel → loopback port

```ini
[kevy]
accept      = 0.0.0.0:6379
connect     = 127.0.0.1:6004
cert        = /etc/kevy/tls/server.crt
key         = /etc/kevy/tls/server.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
requireCert = yes
```

### HAProxy → unix socket

The socket file can be fenced with filesystem permissions: kevy creates
it world-writable, so put it in a directory only the terminator's group
can enter, such as `/run/kevy` with mode `0750` (see [uds.md](uds.md)).
That does not make the terminator the only way in — the loopback TCP
listener below stays up, and on a shared host any local user can still
reach `127.0.0.1:6004`.

```
listen kevy
    bind :6379 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    timeout client 0
    timeout server 0
    server kevy unix@/run/kevy/kevy.sock
```

`server.pem` is the certificate followed by its key in one file.

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
        ssl_certificate        /etc/kevy/tls/server.crt;
        ssl_certificate_key    /etc/kevy/tls/server.key;
        ssl_client_certificate /etc/kevy/tls/ca.crt;
        ssl_verify_client      on;
        proxy_pass kevy;
        proxy_timeout 1h;
    }
}
```

Note the timeouts in all three: a blocking `BLPOP` or an idle Pub/Sub
subscriber holds a connection open with no bytes on it for as long as
the application wants. A proxy that reaps idle connections will look
exactly like kevy dropping subscribers. nginx's `proxy_timeout` cannot
be switched off; set it above the longest idle period you expect.

## Client certificates

The CA only has to be trusted by the terminator, so a private one is
enough. With OpenSSL:

```console
# the CA — keep ca.key offline
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=kevy-ca" \
  -keyout ca.key -out ca.crt

# the terminator's certificate, for the name clients connect to
openssl req -newkey rsa:2048 -nodes -subj "/CN=kevy.internal" -keyout server.key -out server.csr
printf 'subjectAltName=DNS:kevy.internal\n' > san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
  -extfile san.ext -out server.crt
cat server.crt server.key > server.pem      # the HAProxy form

# one certificate per application
openssl req -newkey rsa:2048 -nodes -subj "/CN=billing-app" -keyout billing.key -out billing.csr
openssl x509 -req -in billing.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 825 \
  -out billing.crt
```

Revoking one application means reissuing from a new CA or adding a CRL
to the terminator; kevy itself never sees the certificate.

## The client side

Any stock Redis client with TLS enabled talks to a terminated kevy
unchanged. It needs the CA to trust the terminator and its own
certificate to be let in:

```console
redis-cli --tls --cacert ca.crt --cert billing.crt --key billing.key -h kevy.internal -p 6379 PING
```

**`kevy-cli` cannot.** It rejects `rediss://` with `Unsupported`,
because kevy ships without TLS and the CLI has no TLS stack to lend it.
That is an operational consequence worth planning for rather than
discovering: administer from the host itself, over kevy's own encrypted
client port (`kevy-cli -u kevys://…`, see
[encrypted-links.md](encrypted-links.md)), or over an SSH tunnel:

```console
ssh -N -L 6004:127.0.0.1:6004 you@host   # then: kevy-cli -p 6004
```

## Only expose what is necessary

With the default bind, **kevy needs no firewall rule at all** — it is
not reachable off-host. The terminator's port is the only one to open.
If you must bind kevy to a real interface, then the firewall is doing
the job the loopback bind was doing for free, and it is the only thing
between the network and an unauthenticated database.

## Cluster mode behind a proxy

A key-aware client learns where each slot lives from `CLUSTER SLOTS`
and follows `-MOVED` redirects, so kevy must advertise the addresses
the **proxy** listens on, not its own. Two settings do that:

```toml
[cluster]
enabled            = true
announce_ip        = "203.0.113.7"   # the address clients reach the proxy at
announce_port_base = 7001            # the proxy's port for shard 0
```

Then map the per-shard ports one to one, in the same order: proxy
`7001 + i` in front of kevy's `port + 1 + i`. With two shards behind
HAProxy:

```
listen kevy-main
    bind :7000 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6004
listen kevy-shard0
    bind :7001 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6005
listen kevy-shard1
    bind :7002 ssl crt /etc/kevy/tls/server.pem ca-file /etc/kevy/tls/ca.crt verify required
    mode tcp
    server kevy 127.0.0.1:6006
```

`CLUSTER SLOTS`, `CLUSTER NODES`, `CLUSTER SHARDS` and every `-MOVED`
now name `203.0.113.7:7001` and `203.0.113.7:7002`. Without
`announce_ip` kevy advertises its bind address, and `127.0.0.1` for a
`0.0.0.0` bind — correct for clients on the same host, unreachable from
anywhere else.

## Replication and election between hosts

Replication and the election control plane have their own ports. kevy
can encrypt and authenticate both itself — see
[encrypted-links.md](encrypted-links.md), which needs no tunnels. Without
that, neither is encrypted or authenticated. Across hosts, give every node a
local tunnel endpoint for each of its peers and let every node listen
only on loopback. Each node then needs:

- inbound, one TLS service per local port it serves: the client port
  (for `FAILOVER` probes), the election port, and one replication port
  **per shard**;
- outbound, one client-mode service per peer for each of those ports,
  on a local port of your choosing;
- a `peers` list that names every peer by its **local** tunnel ports,
  using the fourth field for the replication port, so a node following
  a newly elected primary dials the tunnel rather than the peer.

For node `n2` of three, one shard each, peers `n1` and `n3` reached on
local ports `8010-8012` and `8030-8032`:

```toml
[server]
bind = "127.0.0.1"
port = 6004

[replication]
role     = "replica"
upstream = "127.0.0.1:8012"          # n1's replication port, through the tunnel

[cluster]
enabled         = true
node_id         = "n2"
elect_port_base = 6204
peers = "n1@127.0.0.1:8011:8010:8012,n2@127.0.0.1:6204:6004:16004,n3@127.0.0.1:8031:8030:8032"
```

```ini
foreground = yes

; inbound: what peers reach on this host
[in-client]
accept      = 0.0.0.0:7004
connect     = 127.0.0.1:6004
cert        = /etc/kevy/tls/server.crt
key         = /etc/kevy/tls/server.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
requireCert = yes
; [in-elect] 7204 -> 6204 and [in-repl] 17004 -> 16004, same options

; outbound: n1's three ports as local ports on this host
[to-n1-client]
client      = yes
accept      = 127.0.0.1:8010
connect     = n1.internal:7004
cert        = /etc/kevy/tls/n2.crt
key         = /etc/kevy/tls/n2.key
CAfile      = /etc/kevy/tls/ca.crt
verifyChain = yes
checkHost   = n1.internal
; [to-n1-elect] 8011 -> n1.internal:7204, [to-n1-repl] 8012 -> n1.internal:17004,
; and the same three for n3 on 8030-8032
```

Put the TLS options in every service, as above. stunnel 5.76 crashed on
start when they sat in the global section of a file that also had
client-mode services.

Verified: three nodes, one shard each, every link through stunnel with
client certificates. Writes on the primary reached both replicas; a
capture on the network between the hosts never contained a marker value
written 200 times, while a capture on the primary's loopback, taken at
the same time, contained it 600 times; after the primary was killed,
`n2` was elected in 6 s and `n3` followed it through its local tunnel
and received writes made after the failover. Not verified: more than
one shard per node.

## What was verified, and what was not

Measured against this tree, with HAProxy 3.4.5, nginx 1.30.5 and
stunnel 5.76 in front of kevy, redis-cli 8.0.2 as the client and
OpenSSL 3.5.7 issuing the certificates:

- the port surface in the table above, including `--cluster` opening
  `port+1+i` per shard;
- the three client-certificate configurations above, each forwarding to
  kevy's TCP port: a client with a certificate from the CA reads and
  writes; a client with no certificate, and one with a certificate from
  a different CA, are disconnected before sending a command — keys they
  tried to write are absent afterwards;
- the HAProxy and nginx blocks forwarding to kevy's unix socket, with
  the same accept and reject results; the socket kevy created was
  `srwxrwxrwx`, which is why the directory has to do the fencing.
- a `BLPOP` held idle for 10 minutes through each of the three
  terminators with the timeouts above, then released by a push: all
  three returned the pushed element;
- cluster mode through HAProxy with `announce_ip` and
  `announce_port_base`: `-MOVED` names the proxy's shard port, and
  `redis-cli -c` writes keys through one shard port and reads every one
  of them back through the main port;
- stock Caddy (2.11.4) ships no layer-4 module;
- `kevy-cli` rejecting `rediss://`.

Not tested: revoking a client with a CRL, and an expired client
certificate.

## See also

- [uds.md](uds.md) — the unix socket in detail
- [cluster.md](cluster.md) — what single-node cluster mode is for
- [tuning.md](tuning.md) — `--threads`, and why fewer can be faster
