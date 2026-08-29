# FINDING 2026-08-28 — the wire differential reaches the extension route, and finds a defect, a drift, and a gap

**Status**: the defect is fixed, the drift is closed, the gap is named.
`crates/kevy/tests/differential_wire_vs_embedded.rs`.

**Update, same day**: the architectural observation at the end of this
file — that the facade had to restate the server's arity as literals
because steel may not depend on cement — is now closed too. The arity
column lives in `kevy_resp::verb_arity`, which both surfaces can read;
the facade's two `argv.len() < 4` guards call `arity_ok`; and
`tests_verb_meta.rs` holds the two tables in exact bijection, both
directions. Only arity moved: the rest of a VERB_META row is 48 KB of
documentation prose that every wasm build linking kevy-resp would
otherwise carry for nothing.

## Why a second harness

The in-process differential
(`differential_server_vs_embedded.rs`) reached 66 of 70 commands and could
not settle three of the four that differed. All three were the same shape:
`cmd_resolve.rs` routes IDX.QUERY, IDX.LIST and VIEW.LIST to
`Route::Extension`, which is a **scatter-gather across every shard**
(`kevy-rt/src/exec_build.rs:101`) rather than a call the bare
`KevyCommands` dispatcher can make.

So the second harness drives a real server over RESP. Two decisions make it
mean something:

- **One shard.** With `shards(1)` the scatter-gather degenerates to a single
  participant and the two sides answer the same question. At eight shards a
  gathered result may legitimately order differently, and a byte comparison
  would report arithmetic as divergence.
- **A length-aware reader.** The sibling e2e tests sleep 30 ms and read
  once, which is fine when a test knows what it asked for. Across a corpus a
  short read manufactures divergence out of nothing, so the reply is parsed
  and read until complete.

## 1. A defect in the server, generalised from one verb to twelve

First run: `IDX.QUERY t *` came back `-ERR unknown command 'IDX.QUERY'`.

The command exists. `cmd_resolve.rs:184` reads `b"IDX.QUERY" if args.len()
>= 4 => Route::Extension` — three arguments miss the guard, the arm falls
through, and the local dispatch chain does not carry extension verbs, so it
reports the verb as unknown.

The pattern is the whole family. Probing every arity-guarded verb with the
wrong count:

**12 of 14 reported a command that exists as one that does not** — every
IDX.\*, every VIEW.\*, and PREFIX.DIGEST. Only the TABLE.\* pair escaped,
via a different fallthrough.

**The information was already there.** `verb_meta` carries an arity for all
of them (`IDX.QUERY` is `-4`, `IDX.LIST` is `1`), and the MULTI queue path
in `commands.rs:422` already consults it. The main dispatch site simply
never asked. It asks now — `cmd::unhandled_verb` — and all 14 answer with
Redis's wording: `ERR wrong number of arguments for 'idx.query' command`.

Control assertions in the same test: a verb that genuinely does not exist
still reports unknown, and a correct call is not turned into an arity error.

## 2. A drift between the two surfaces

With the server fixed, the same commands still disagreed — in wording:

| | |
|---|---|
| server | `ERR wrong number of arguments for 'idx.query' command` |
| facade | `ERR IDX.QUERY 't': bad arguments — run COMMAND DOCS IDX.QUERY for the syntax` |

Both honest, and different. `dispatch_argv` is the entry every language
binding is built on, so a binding user and a server user were being told
different things about the same typo.

The facade's entry checks now use the server's arity and the server's
sentence. Its `badargs` is kept for what it was always right for: arguments
that are **present and wrong** — a shape that will not parse, a range that
will not read. The two had been one message.

23 of 26 now agree byte for byte, up from 20.

## 3. A gap that is not a defect

The three that remain are a capability difference:

| | IDX verbs |
|---|---|
| embedded facade | ADVISE, COUNT, CREATE, DROP, LIST, QUERY |
| server extension route | COUNT, EXPLAIN, LIST, QUERY, REBUILD, VERIFY |

Each has one the other lacks. `IDX.EXPLAIN`, `IDX.VERIFY` and `IDX.REBUILD`
are server-only; `IDX.ADVISE` is facade-only. These are named in the
harness's register with that reason. They are **two surfaces that drifted**,
which is a different problem from one capability implemented twice, and the
answer is a decision about scope rather than a deletion.

## What this says about the duplication question

The clone atlas put `kevy-embedded ↔ kevy` at 35 of the top 60 cross-crate
pairs and 751 shared fingerprints, across roughly 9,400 lines, and asked
whether that is redundancy. Two harnesses now answer, for everything they
can reach: **the outputs agree byte for byte** — 66 of 70 in process, 23 of
26 over the wire — and every exception is named. That is evidence for
redundancy rather than for two designs.

It is not yet a claim about the whole surface. Coverage is roughly a quarter
of the verb-shaped literals in the two dispatch trees, and the three named
gaps sit exactly where the atlas's matches were densest.

## An architectural observation, with its evidence

The arity fix could not be shared. `verb_meta` — the single source of truth
for every verb's arity, flags, syntax and docs — lives in `kevy`, which is
**cement**. `kevy-embedded` is **steel**, and steel may not depend on
cement, so the facade had to restate the two arity numbers it needed as
literals in its own entry checks.

That is the drift in section 2 waiting to happen again. A verb table that
both surfaces read is the structural fix; moving it is a real change and is
not made here. Recorded so the next person does not rediscover it by
finding the third divergence.
