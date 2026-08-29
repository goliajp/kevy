# FINDING 2026-08-27 — the embedded facade answers what the server answers, byte for byte

**Status**: the clone atlas's dominant signal now has a measurement against
it. `crates/kevy/tests/differential_server_vs_embedded.rs`.

## The question the atlas raised

`bench/FINDING-2026-08-27-the-clone-atlas-names-one-dominant-twin.md` found
`kevy-embedded ↔ kevy` at 35 of the top 60 cross-crate pairs and 751 shared
fingerprints, an order of magnitude past anything else, matching command by
command across ~4,077 and ~5,336 lines. It also said plainly that shape
matching is not proof two implementations agree, and that a differential
harness is what could settle it.

## Why a harness is cheap here

Both sides expose the same observable, which is the whole reason this was
worth doing rather than guessing:

| | entry point | produces |
|---|---|---|
| server | `kevy::KevyCommands::dispatch(&mut Store, argv)` | `Vec<u8>` of RESP |
| embedded | `kevy_embedded::Store::dispatch_argv(argv, &mut out)` | RESP into `out` |

An argv in, RESP bytes out. No socket, no server process, no interpretation
— the comparison is `==` on two byte vectors.

## The result

A 70-command corpus, ordered and stateful, weighted toward the surface the
atlas flagged (index, zset, view) and toward error shapes, since an error
string is where a second implementation is most likely to have been written
rather than shared:

**66 of 70 commands agree byte for byte.**

The four that differ, each checked against the source before being written
down:

| command | server | embedded | what it is |
|---|---|---|---|
| `SUBSCRIBE` | arity error | unknown command | **a real difference, and the right one** |
| `IDX.LIST` | unknown command | `*0` | a boundary of the harness |
| `IDX.QUERY t *` | unknown command | syntax error | a boundary of the harness |
| `VIEW.LIST` | unknown command | `*0` | a boundary of the harness |

**SUBSCRIBE is a genuine and correct divergence.** The embedded facade
exposes subscription as a typed API — `ops.rs:439`, `pub fn subscribe(&self,
channels) -> Subscription` — rather than as a RESP verb, because an
in-process caller holds the `Subscription` object and there is no connection
whose state `SUBSCRIBE` could change. The server answers with an arity error
precisely because it does know the verb.

**The other three are my harness, not the product.** `cmd_resolve.rs` routes
`IDX.QUERY` (line 184), `IDX.LIST` (189) and `VIEW.LIST` (191) to
`Route::Extension`, and the bare `KevyCommands` dispatcher this harness
drives does not carry the index runtime. The server implements all three
(`cmd_index_query.rs:86` and `:105`, `cmd_view_reduce.rs:182`). I nearly
recorded these as "the server lacks index commands", which would have been
wrong and would have pointed the next reader at nothing.

That distinction is why the register stores a *reason* rather than just an
entry. Two of these four rows describe the instrument; one describes the
design; none describes a defect.

## What it means for v6

On this evidence the duplication the atlas found is **redundancy, not
divergence**: two implementations of one command surface producing identical
bytes on everything the harness could reach. That makes unification a real
option rather than a hopeful one, and it is the first evidence any
instrument here has produced for the "no complex implementation where a
simpler one delivers the same capability" goal.

## What this does not establish

**Coverage is roughly a quarter.** The corpus is 70 commands. Grepping both
dispatch trees for verb-shaped string literals gives 194 on the embedded
side and 207 on the server's, 122 in common — an overcount, since argument
keywords like `WITHSCORES` and `MATCH` match the same pattern, but the right
order of magnitude. Most of the surface has not been compared.

**Three of the four divergences are unexamined**, since the harness cannot
reach the extension route. Whatever the index and view commands do
differently, if anything, is exactly where the atlas's densest matches were.

So the honest statement is: everything this harness could compare, agrees.
Extending it to the extension route is what would turn that into a claim
about the whole surface, and it is the obvious next step.

## Verified red before green

Removing one named divergence fails the test with "1 command(s) diverge
without a stated reason"; adding two commands to the corpus moves the count
from 70 to 72 and still passes; restoring returns 66/70. The harness also
asserts its own corpus ran, so a harness that measured nothing cannot read
as agreement.
