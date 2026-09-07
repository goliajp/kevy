# Upgrading from 6.2 to 6.3

The short version: **nothing you have written stops working.** No API
moved, no crate changed shape, the data directory opens in both
directions, and a 6.2.x replica talks to a 6.3.0 primary. Bump the
number and you are done.

```toml
kevy-embedded = "6.3.0"
```

The rest of this page is about what became *possible* in 6.3.0, which
scenario each addition is for, and the one reply that changed because it
was wrong before.

## TL;DR — what to do

| If you… | Action |
|---|---|
| run the server or a binding | swap the binary / bump the package; nothing else |
| embed from Rust | bump `kevy-embedded` to `6.3.0` |
| sample fields from a hash with `HGETALL` | you can now use `HRANDFIELD` — §3 |
| use a client that sets `notify-keyspace-events` on connect | it now works; delete any config-file workaround — §1 |
| negotiate `HELLO 3` **and** decode by type | five replies now carry their real types — §2 |
| handle errors from `GEOPOS` | one malformed reply is now a plain error — §4 |
| pull `ghcr.io/goliajp/kevy:latest` | pull again; the tag now resolves to 6.3.0 |

---

## 1. `CONFIG SET notify-keyspace-events` reaches the engine

**The scenario.** You run a library that subscribes to key-expiry or
keyspace events — Spring Data Redis's `RedisKeyExpirationEvent`, the
socket.io Redis adapter, several job queues — or you want
expiry/eviction notifications and were setting them in the config file
because setting them over the wire did not work.

**What was wrong.** Keyspace notifications have worked for a long time,
but only from the config file, where the key is spelled
`notify_keyspace_events` with underscores. Redis spells it hyphenated on
the wire, and nothing bridged the two: the parameter was neither
readable nor writable on a connection. A library that sets it on connect
— which is the normal way — met `ERR unknown parameter` in its first
second, on a feature the engine had all along.

**What to do.** Nothing, unless you had worked around it. If you were
setting it in the config file to serve a client that wanted to set it
itself, you can delete the workaround and let the client do its job.

```
CONFIG SET notify-keyspace-events Ex     → +OK
CONFIG GET notify-keyspace-events        → "Ex"
CONFIG SET notify-keyspace-events ZZZ    → -ERR CONFIG SET failed for
                                           'notify-keyspace-events': unknown flag char 'Z'
```

The flag set is Redis's: `K` keyspace, `E` keyevent, `g` generic, `$`
string, `l` list, `s` set, `h` hash, `z` zset, `t` stream, `x` expired,
`e` evicted, `n` new-key, and `A` as the alias for `g$lshzxet` (every
class except `n`, per the Redis contract for `A`). Flags are validated
on write rather than stored and silently ignored, so a typo is an error
at the moment you make it, not silence at the moment you needed an event.

---

## 2. Five RESP3 replies now carry their real types

**The scenario.** Your client negotiates RESP3 — `HELLO 3` — and decodes
by reply type. That is redis-py with `protocol=3`, node-redis v5 and
later by default, and anything built on them. **If your client speaks
RESP2, which is still the default in most setups, nothing in this
section affects you.**

**What was wrong.** Five commands sent their RESP2 shapes to a
connection that had negotiated RESP3:

| command | was sending | now sends, as Redis 8.10.1 does |
|---|---|---|
| `ZPOPMIN` | score as a bulk string | score as a **double** |
| `ZADD … INCR` | new score as a bulk string | new score as a **double** |
| `GEOPOS` | coordinates as bulk strings | coordinates as **doubles** |
| `SPOP key count` | an **array** | a **set** |
| `HRANDFIELD … WITHVALUES` | a flat list | **nested pairs** |

A type-decoding client was therefore handed strings where it expected
numbers, and a list where it expected a set. Most clients coerce, so
this surfaced as subtly wrong types in your data rather than as an
error — which is why it went unnoticed for as long as it did.

**What to do.** If you were coercing these yourself — `float(score)`
after a RESP3 `ZPOPMIN`, say — the coercion is now redundant but
harmless. If you have a golden-file or snapshot test that recorded the
RESP2 shapes over a RESP3 connection, re-record it: that test was
pinning a defect.

**How this was found**, because it says something about what to trust:
`bench/resp3gate.sh` asks the pinned Redis which verbs change shape
under `HELLO 3`, and requires kevy to move exactly where Redis moves. It
is not a hand-written list of commands we believe are RESP3-aware — the
list comes from the opponent, on every run, and the gate refuses to pass
if it finds implausibly few. Eleven verbs change shape; kevy now
disagrees on none.

---

## 3. `HRANDFIELD` is implemented

**The scenario.** You sample fields out of a hash — feature flags, A/B
buckets, sharded work queues, "show me a few of these" endpoints — and
were pulling the whole hash with `HGETALL` to pick from it in your own
code.

```
HRANDFIELD key                      → one field
HRANDFIELD key 5                    → up to 5 DISTINCT fields (capped at the hash size)
HRANDFIELD key -5                   → exactly 5, REPEATS ALLOWED
HRANDFIELD key 5 WITHVALUES         → each field with its value
```

**The sign of the count is the whole API.** Positive means a *subset*:
distinct fields, and you get fewer than you asked for when the hash is
smaller. Negative means a *sample*: exactly `|count|` entries, and the
same field may come back twice. That is Redis's convention and kevy
follows it exactly, including the empty array for a missing key and for
a count of zero.

**What it saves.** `HGETALL` on a large hash puts the entire hash on the
wire so you can pick three fields out of it. `HRANDFIELD key 3` puts
three on the wire. On a hash of a few thousand fields sampled once per
request, that is the difference between a kilobyte and a few dozen bytes
on every call, and it is the reply that dominates — the selection itself
is a partial Fisher-Yates over the field list, shuffling only the prefix
that gets returned.

It works on all four of kevy's hash representations, packed rows
included, so you do not have to know which one a given key is using.

---

## 4. `GEOPOS` on a wrong-typed key

**This is the one reply that changed, and the only place existing code
could notice.**

Before, asking `GEOPOS` about a key that holds a string answered with an
array header *and then* an error — `*1\r\n-WRONGTYPE …` — so a client
reading that reply saw an array whose first element was an error. The
header had already gone out by the time the type was resolved.

Now it answers `-WRONGTYPE Operation against a key holding the wrong
kind of value`, and nothing else, exactly as Redis answers.

**Who notices.** Code that checked `reply[0]` for an error marker rather
than checking whether the reply *is* an error. That code now sees the
error where errors belong. Most client libraries were already treating
the old shape as malformed, so in practice this fixes handling rather
than breaking it.

The other three cases are unchanged: a real member gives its
coordinates, and a missing member and a missing key both give the null
array.

---

## What carries over unchanged

- **The wire, apart from §2 and §4.** Every other RESP2 and RESP3 reply
  is byte-identical to 6.2.2.
- **The data directory.** AOF, snapshots, the value log and every
  checkpoint open exactly as before, in both directions. There is no
  migration step and no one-way door.
- **Replication.** The stream is unchanged: `HRANDFIELD` is a read and
  never enters it, and nothing else in this release touched the
  replication path. A 6.2.x replica and a 6.3.0 primary pair in either
  direction, so you can upgrade one node at a time.
- **Every crate and binding API.** They move from 6.2.2 to 6.3.0 with no
  code change.

---

## What else 6.3.0 changed — measurement, not behaviour

No behaviour depends on any of this, but it changes how much the
published numbers are worth.

- **Every benchmark opponent is pinned to an exact version, and checked
  against its upstream's latest stable.** Redis 8.10.1, valkey 9.1.2,
  Dragonfly 1.40.2, postgres 18.6, plus the four client libraries the
  conformance suite drives (go-redis 9.22.0, StackExchange.Redis 3.1.31,
  node-redis 6.2.1, redis-py 8.1.0). Two of those clients had been
  frozen since 2024 and two carried no version at all. The harness now
  asks each engine what version it is and refuses to produce a number on
  a mismatch, so a published table names its opponent to the patch.
  Raising a pin is a documented procedure rather than an edit —
  `.claude/skills/competitor-anchors/SKILL.md`.
- **Every command the pinned Redis serves is now accounted for.** Of its
  599 commands and subcommands, kevy implements 206 verbs; of the rest,
  256 are exempt with a written reason each and 80 are owned by a named
  RFC. Nothing is unclassified, and that count sits on a ratchet that
  can only go down — so a future Redis release adding commands lands as
  a decision to make rather than as silence. If you have wondered
  whether some verb you need is missing, the answer is now written down
  in `bench/COMMAND-COVERAGE.json` instead of being discoverable by
  trying it.

The measured numbers for 6.3.0 — lx64, three full runs, per-cell median,
each engine's own command counter, `-c 50 -P 16`:

| verb | kevy | Redis 8.10.1 | vs Redis |
|---|---:|---:|---:|
| GET | 7,489,119/s | 5,631,398/s | 1.33x |
| SET | 6,824,662/s | 2,567,607/s | 2.66x |
| INCR | 6,753,558/s | 3,294,927/s | 2.05x |
| SADD | 6,152,617/s | 3,753,131/s | 1.64x |
| HSET | 4,002,580/s | 2,966,288/s | 1.35x |
| ZADD | 3,242,967/s | 2,818,626/s | 1.15x |
| LPUSH | 3,142,699/s | 2,860,306/s | 1.10x |

kevy's worst run beats every competitor's best run in all seven cells;
the narrowest are LPUSH and ZADD at 1.08x against Redis. The serving
path did not change in this release, so these are 6.2.2's numbers
re-measured rather than improved — the full entry, including valkey and
Dragonfly and the run-to-run spread, is in `bench/PERF-LEDGER.md`.

---

## Getting it

```toml
# Cargo.toml
kevy-embedded = "6.3.0"
```

```sh
npm install @goliapkg/kevy@6.3.0          # wasm
npm install @goliapkg/kevy-node@6.3.0     # Node native
npm install @goliapkg/kevy-bin@6.3.0      # the server binary
pip install kevy==6.3.0
go get github.com/goliajp/kevy-go/v6@v6.3.0
```

Container users on `ghcr.io/goliajp/kevy:latest` get 6.3.0 on the next
pull. If you pin a tag, it is `ghcr.io/goliajp/kevy:6.3.0`.
