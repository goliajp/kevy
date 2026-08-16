# Upgrading from 5.1 to 5.2

The short version: **stop the 5.1 server, start the 5.2 binary on the
same data directory.** No config changes, no wire changes, no client
changes. The data directory opens in both directions.

**Read the Lua section if you run scripts.** The embedded Lua runtime
moved three minor versions, and it corrected the 5.1 and 5.2 dialects
against upstream Lua. kevy defaults to Lua 5.1, so those corrections are
visible through `EVAL` without anyone opting in — including one that can
stop an existing script from finding a function it uses.

## Why this release exists

- **The browser build carries the whole embedded surface.** It was
  compiled with `core` and `persist` only, so `IDX.*`, `VIEW.*` and
  `TABLE.*` answered `unknown command` in a browser — on a page whose
  argument is that kevy does secondary indexes, full-text and vector
  search inside the engine. `index`, `text` and `vector` are on now.
  The module goes from 726 KB to 1441 KB (481 KB gzipped).

  What stays out needs something a browser cannot provide rather than
  bytes saved: `replicate` a network peer, `listener` a TCP socket,
  `tier` a disk directory. Streams, transactions, geo and scripting are
  outside the embedded engine's verb surface on every platform.

- **A denial of service in the Lua runtime is fixed.**
  `string.rep("", math.maxinteger, "")` looped without allocating, so
  the size guard could not catch it: one expression hung the VM. If you
  run untrusted Lua, this is the reason to take 5.2.

- **The Lua dialects match upstream Lua.** Details below — this is the
  part that can change what an existing script does.

## What carries over unchanged

- **The wire.** RESP2 and RESP3 replies are byte-identical. No client
  needs a new version.
- **The data directory.** AOF, snapshots, the value log, index
  checkpoints and the catalog all open as they are. 5.1 can open a
  directory 5.2 has written.
- **Configuration.** Every key keeps its name, type and default.
- **Replication.** A 5.1 replica follows a 5.2 primary and the reverse,
  so a rolling upgrade needs no particular order.

## What behaves differently

Everything here comes from the Lua runtime moving to luna-core 3.0.0,
which corrected the 5.1 and 5.2 dialects against PUC Lua 5.5.1. kevy's
default dialect is 5.1.

### The table library follows the dialect

Under the 5.1 default, the standard library now offers what Lua 5.1
actually has:

| | 5.1 (kevy 5.1) | 5.2 (kevy 5.2) |
|---|---|---|
| `table.setn` | absent | present — raises `'setn' is obsolete`, as upstream does |
| `table.unpack` | present | **absent** |
| `table.create` | present | **absent** |

The runtime used to register the union of every version's functions, so
a script could call names its target dialect does not have. **A script
using `table.unpack` under the default dialect will stop finding it.**

Two ways forward, both verified:

- Use the 5.1 spelling — the global `unpack`, which is unaffected:

  ```lua
  local t = {1, 2, 3}
  return {unpack(t)}
  ```

- Or ask for a dialect that has `table.unpack`:

  ```lua
  #!lua version=5.4
  return {table.unpack({1, 2, 3})}
  ```

### Type errors name the operand first

Upstream Lua ≤ 5.2 puts the operand before the type; 5.3 flipped to
type-first. kevy emitted the 5.3+ shape on every dialect. Under 5.1 and
5.2 the messages now read as upstream writes them:

| | 5.1 (kevy 5.1) | 5.2 (kevy 5.2) |
|---|---|---|
| calling a nil local | `attempt to call a nil value (local 'f')` | `attempt to call local 'f' (a nil value)` |
| indexing a nil local | `attempt to index a nil value (local 't')` | `attempt to index local 't' (a nil value)` |
| arithmetic on a nil local | `…on a nil value (local 'a')` | `…on local 'a' (a nil value)` |

This matters if a script matches on the text of an error it catches, or
if an application asserts on the string a failed `EVAL` returns. The
error *class* is unchanged; only the wording moved.

Dialects 5.3, 5.4 and 5.5 are untouched.

## If you embed the store

`kevy-embedded` and the language bindings all move to 5.2.0 together.
The Lua behaviour above applies wherever `EVAL` is reachable.

The browser package `@goliapkg/kevy` gains `IDX.*`, `VIEW.*` and
`TABLE.*` through `cmd`. Its typed surface is unchanged, and a store
written by 5.1 in OPFS or IndexedDB opens under 5.2.

## Recommended procedure

1. **Look for `table.unpack`, `table.create` and `table.setn` in your
   scripts.** Under the default dialect the first two stop resolving;
   the third starts existing and raises the obsolescence error upstream
   raises. If you find any, pick one of the two fixes above before
   upgrading — both work on 5.1 too, so the change is safe to make
   first.
2. **Look for anything matching on the text of a Lua error.** The
   wording under 5.1 and 5.2 changed; the classes did not.
3. Stop the 5.1 server. Start the 5.2 binary on the same `--dir`.
4. `INFO server` reports `kevy_version:5.2.0`.

Rolling back is stopping 5.2 and starting 5.1 on the same directory.
Nothing 5.2 writes is unreadable to 5.1.
