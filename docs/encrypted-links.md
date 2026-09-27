# Encrypted links

Replication, the election control plane and client connections can each
run over kevy's own encrypted, authenticated links. All of them are **off
unless you turn them on**. There is still no TLS and no AUTH: a client that
needs TLS goes through a proxy as
[deploy-behind-a-proxy.md](deploy-behind-a-proxy.md) describes, and the
plaintext client port behaves the same whether or not the encrypted one is
open.

## What it covers

| Link | Encrypted when | Who is authenticated |
|---|---|---|
| election (`elect_port_base`) | `[cluster] secure = true` | both ends, by their keys in `peer_keys` |
| replication (`listen_port_base + i`) | `[replication] secure = true` | the primary by `upstream_key` or a peer key; the replica by `replica_keys` when set |
| clients on `[secure] listen_port` | always | the server by its key; the client by `client_keys` when set |
| clients on `port`, cluster ports, unix socket | never | nobody — use a proxy |

The protocol is Noise `IK` with X25519, ChaCha20-Poly1305 and BLAKE2s. The
initiator knows the responder's public key before it connects, the
responder learns the initiator's from the first message and can refuse it
before answering, and everything after that single round trip is
encrypted and authenticated. The primitives are kevy's own, with no
dependencies; they have been checked against the published test vectors
and against other implementations, and have not been audited by a third
party.

## Keys

Every node needs a key pair. `kevy keygen` writes the private key and
prints the public one:

```console
$ kevy keygen /etc/kevy/node.key
ffe2a40f453275a19e8b115332459968e9a3b54b9c2e728ec538ccdd724b2f6c
```

The file is created with mode `0600` and is never overwritten. kevy
refuses to start with a key file that other users can read.

## Configuration

Each node names its own key, and lists every peer's public key, as
`kevy keygen` printed it on that node. This is the file for `n2`:

```toml
[secure]
private_key_file = "/etc/kevy/node.key"

[cluster]
enabled   = true
node_id   = "n2"
secure    = true
peers     = "n1@10.0.0.11:6204:6004,n2@10.0.0.12:6204:6004,n3@10.0.0.13:6204:6004"
peer_keys = ["n1=d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c", "n3=38bde379dfddf094d8746267013da2d5b62153c74dea6f82a4910797faecb440"]

[replication]
role         = "replica"
upstream     = "10.0.0.11:16004"
secure       = true
upstream_key = "d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c"
replica_keys = []   # on a primary: the replica keys it accepts; empty accepts any
```

- `peer_keys` needs an entry for every other node in `peers`; the node's
  own entry is ignored, so one list can be shared by every node.
- A replica trusts `upstream_key` and every key in `peer_keys` as a
  primary. After an election it follows the new primary without any
  change, because that node's key is already in the list.
- `replica_keys` on a primary restricts which replicas may connect. Left
  empty, any replica may connect and the link is still encrypted.

kevy checks this at startup. A link with `secure = true` but no
`private_key_file`, a peer without a key, or a secure replica without
`upstream_key` stops the node with a message naming what is missing. It
never falls back to plaintext.

## Embedded stores

An embedded writer and its replicas encrypt their link the same way,
configured in code instead of `kevy.toml`. The handshake is the same as
the server's, so an embedded replica can follow a secure kevy server
and a server replica can follow a secure embedded writer.

```rust
use kevy_embedded::{Config, Keypair, LinkKeys, Store};

# fn keys() -> (Keypair, Keypair) { (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32])) }
let (writer_key, replica_key) = keys();
let writer_public = writer_key.public();

// the writer: any replica may connect, still encrypted
let writer = Store::open(
    Config::default()
        .with_embed_writer("0.0.0.0:7101")
        .with_writer_security(LinkKeys { local: writer_key, peers: vec![] }),
)?;

// a replica: trusts the writer's public key
let replica = Store::open(
    Config::default()
        .without_aof()
        .with_replica_upstream("writer.internal:7101")
        .with_replica_security(LinkKeys { local: replica_key, peers: vec![writer_public] }),
)?;
# Ok::<(), kevy_embedded::KevyError>(())
```

- On a replica, `peers` lists the primaries it trusts, tried in turn
  until one answers; opening fails when it is empty. On a writer, it
  lists the replicas it accepts; empty accepts any.
- `Keypair::from_secret` takes the 32 bytes that `kevy keygen` writes
  as hex; where the application keeps them is up to it.

## Clients

`[secure] listen_port` opens a second client port that speaks only the
encrypted protocol; the plaintext `port` stays as it is. Clients connect
with a `kevys://` URL naming the server's public key:

```toml
[secure]
private_key_file = "/etc/kevy/node.key"
listen_port      = 6404
client_keys      = []   # client public keys allowed; empty accepts any, still encrypted
```

```text
kevys://10.0.0.11:6404?server_key=d5c015b88401b6b33f5cb292b01ff3034e5f1de008b2e3f77faed23c31f16a4c
kevys://10.0.0.11:6404/0?server_key=<hex>&client_key_file=/etc/app/kevy.key
```

- A client without `client_key_file` draws a fresh key pair for each
  connection. That is enough when `client_keys` is empty; when it is not,
  create the client's key with `kevy keygen` and list its public half.
- The Rust clients accept `kevys://`: `kevy-resp-client`
  (`RespClient::connect_url`, or `SecureStream` under any RESP code),
  `kevy-client` (`Connection` and `Subscriber`), and `kevy-client-async`
  (`AsyncConnection::connect_secure_url`,
  `AsyncSubscriber::connect_secure_url`), and so does `kevy-cli -u`. The
  other language bindings do not; use a TLS proxy for them.
- `CLIENT LIST`, `CLIENT INFO` and `CLIENT KILL ADDR` show the client's own
  address, not the server's.
- The port needs `private_key_file`, and must differ from `port`; either
  mistake stops the server at startup.

## Cost

The encryption runs on its own threads beside the reactors: each
connection's bytes are decrypted there and passed to the plaintext port
over loopback, and the replies are sealed on the way back. The plaintext
path is the same code whether or not this port is open, and an encrypted
connection pays for one extra loopback round trip besides the crypto.

Measured on one Linux host, client and server on loopback, four shards:

| | plaintext port | encrypted port |
|---|---:|---:|
| one request, round trip | 10 µs | 25 µs |
| 256 KB `GET`, one connection | 3.0 GB/s | 0.32 GB/s |
| new connection plus `PING` | 38 µs | 0.58 ms |

Most of the round trip is the extra hop; sealing and opening a small
message takes about half a microsecond. Large values are limited by the
cipher, which is a portable implementation; the handshake is dominated by
X25519. Connection pools, which keep connections open, pay the handshake
once.

## Mixing secure and plain nodes

A secure node does not talk to a plain one. A plain replica connecting to
a secure primary gets no answer, and a node whose key is not in the
others' `peer_keys` is not heard in elections. Turn a cluster secure by
restarting every node with the new configuration.

## What was verified

On three nodes, one shard each, election and replication both secure:

- writes on the primary reached both replicas, and a capture of the
  network between the nodes contained a marker value written 200 times
  zero times on the replication and election ports; the same run with
  `secure = false` contained it 400 times;
- after the primary was killed, a new primary was elected, the other node
  followed it and received writes made after the failover;
- with four shards per node on the io_uring reactor, a replica restricted
  by `replica_keys` received all 400 keys written, and a capture of the
  replication ports held none of the values; the plaintext run of the
  same setup showed all 400;
- a replica expecting a different primary key, a replica outside
  `replica_keys`, a plaintext replica, and an election peer holding an
  unconfigured key were all refused.

## See also

- [deploy-behind-a-proxy.md](deploy-behind-a-proxy.md) — TLS for clients that cannot use `kevys://`
- [replication.md](replication.md) — replication itself
- [availability.md](availability.md) — elections and failover
