# FINDING 2026-08-27 — the clone atlas names one dominant twin, and one that is deliberate

**Status**: read, not acted on. The v6 toolchain RFC §6 requires this atlas
to be read before any dedup gate is designed, and explicitly keeps "a
register of deliberate twins and no gate at all" as a legitimate outcome.
This is that read.

## Method, and the defect it had first

Winnowing (Schleimer–Wilkerson–Aiken) over 925 files, k=30 tokens, w=20,
identifiers and literals normalised so a renamed copy still matches.

The first version used Python's builtin `hash()`, which is salted per
process. Three runs over an unchanged tree reported **987, 1,034 and 1,011**
pairs. An instrument whose reading moves when nothing moved is not an
instrument; with blake2b-64 the same tree now reports 1,029 pairs, byte
identical across three runs.

**1,029 pairs above the threshold, 246 of them crossing a crate boundary.**

## One pair dominates everything else

Aggregating the top 60 cross-crate pairs by the crates involved:

| crate pair | pairs | shared fingerprints |
|---|---:|---:|
| **kevy-embedded ↔ kevy** | **35** | **751** |
| kevy-persist ↔ kevy-store | 4 | 72 |
| kevy-client-async ↔ kevy-client | 2 | 106 |
| kevy-index ↔ kevy-text | 2 | 47 |
| kevy-persist ↔ kevy-vlog | 1 | 55 |

kevy-embedded ↔ kevy is an order of magnitude past the next entry, and the
matches are not incidental — they line up command by command:

| kevy-embedded | kevy |
|---|---|
| `dispatch/idx_create.rs:57` | `cmd_index.rs:131` |
| `dispatch/idx_query.rs:364` | `cmd_index_query/args.rs:160` |
| `dispatch/zset.rs:23` | `cmd_zadd.rs:67` |
| `dispatch/view.rs:46` | `cmd_view.rs:55` |
| `ops_index_sync.rs:183` | `index_runtime/row_apply.rs:135` |

The scale: `kevy-embedded/src/dispatch/` is 19 files and 4,077 lines;
`kevy/src/cmd_*.rs` is 21 files and 5,336 lines. Roughly 9,400 lines that
may be one command surface implemented twice — the embeddable facade and
the RESP server.

**This is not yet a verdict.** The atlas compares code shape, and shape can
match while behaviour differs: the two sides have different transports,
different argument sources, different error surfaces. What settles it is
F2, the differential harness — drive both with one command corpus and
compare the observable results. Until that runs, the honest statement is
that the largest duplication signal in the codebase points at the
server/embedded pair and nothing has tested whether they agree.

That gives F2 a named target instead of a hypothetical one, which is what
the atlas was for.

## One twin is deliberate, and says so in its own source

`kevy-persist/src/crc32c.rs` and `kevy-vlog/src/crc32c.rs` are 73 lines
each and differ by 16 lines. Both crates already depend on `kevy-sys`,
which exports `crc32c` and `try_crc32c_hw`.

My first reading of this was going to be "two steel crates carry redundant
copies of an algorithm the stone already provides, and the copies are
probably slower". **Both halves of that would have been wrong.** The copies
call `kevy_sys::checksum::try_crc32c_hw` for the hardware path, so they are
not slower; and `kevy-vlog/src/crc32c.rs:3` says outright it is "a copy of
kevy-persist's module (same polynomial, same hw dispatch)", with the reason
stated: **wasm32, where kevy-sys never links.**

So what is actually duplicated is the *software fallback*, twice, because
the stone that would hold it does not exist on one target.

That is a considered twin, not an oversight — and its reason is checkable
and arguably closable: a software CRC32C in a stone that does link on
wasm32 would let both steel crates hold one implementation. Worth doing,
worth doing on purpose, and worth recording either way. It is the first
entry in the register of deliberate twins the RFC anticipated.

## Verdict on a dedup gate

**No gate.** Not yet, and possibly not at all.

- The dominant signal is one architectural pair whose status is unknown
  until F2 runs. Gating on fingerprint counts would fire on it every day
  while telling nobody anything they do not already know.
- The clearest second signal turned out to be deliberate, documented in
  source, and correctly reasoned. A gate would have flagged it as a defect.
- A threshold picked now would be picked from taste. Every other gate in
  this toolchain took its numbers from a measurement first.

What this atlas earns instead: a **register of deliberate twins** —
crc32c's entry is written above — and F2 pointed at kevy-embedded ↔ kevy.

## What the atlas cannot do, stated so nobody expects it to

It finds code that was **copied**. Two *different* implementations of one
capability share no tokens and are invisible to it — and that is the case
the v6 goal is really about. Fingerprints are the cheap half of the
question; the differential harness is the half that answers it.
