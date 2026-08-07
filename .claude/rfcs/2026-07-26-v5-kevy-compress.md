# kevy-compress — a corpus, not a datum

> v5 **experiment** arc, second train. **Status: DESIGN — not approved, no code.**
>
> **Header drift, noted 2026-08-06.** The line above says "not approved,
> no code". Both halves are contradicted by the record: `.claude/ROADMAP.md`
> marks this RFC approved, and a train was built against it on the
> `r1-locality` branch (the allocator reached a v8 closing ledger there).
> This note states what is verifiable rather than rewriting the status —
> whether "approved" is the owner's word is theirs to say. What must not
> stand is a header that sends the next reader looking for permission
> that the work already assumed.
>
> **This is an attempt at v5, not v5** (owner, 2026-07-26). Every claim below is
> a hypothesis under test, revisable down to its premises — including §2's
> corpus-versus-datum argument, which is the one worth killing early if it is
> wrong. §4 names references without promising to reproduce them.
>
> Design input: `.claude/plans/2026-07-26-v5-arc-design-input.md`.
> Companion: [`2026-07-26-v5-kevy-alloc.md`](2026-07-26-v5-kevy-alloc.md).
>
> **Framing (owner, 2026-07-26):** not an improvement — a design. The target
> is the ceiling the model permits, stated structurally, with a headline number
> falling out as a consequence. Criteria below are written that way.
>
> Zero-dep holds: *build it ourselves, learn from the good open source
> projects, and only do it better.* Standing on named shoulders (§4).

## 1. What we do not have, stated honestly

kevy has **no value compression**. The PG comparison
([PGCOMPARE-2026-07-26](../../bench/PGCOMPARE-2026-07-26.md) §"The compression
difference") records the gap and is careful about the number:

> The first attempt at round two used a constant pad (`"x" * 4000`). Past PG's
> ~2 KB TOAST threshold that compresses about **25:1** — 12 GB of CSV became
> 488 MB on PG's disk … Every column of that run was void and it was discarded.

So **25:1 is a constant-pad best case, not a general figure**, and the run that
produced it was thrown away. What survives is the qualitative statement, which
is enough: for a table of JSON or prose, PG's disk footprint can be a fraction
of ours. The published comparison uses random hex precisely so this difference
does not contaminate the other six columns.

Two measured numbers do bound the opportunity:

- **vlog space amplification is 1.27×** at B5, on incompressible payloads.
- **cold read p99 is 145 µs (hash) / 105 µs (scalar)** at B2 — the budget any
  decode step has to fit inside.

## 2. What the model permits — and it is a category difference

PostgreSQL compresses **per datum**. Each TOASTed value is compressed alone,
and below the ~2 KB threshold it is not compressed at all. A 400 B row of JSON
has almost no *internal* redundancy — its redundancy is **across rows**: the
same field names, the same enum values, the same URL prefixes, repeated
millions of times. Per-datum compression cannot see any of it.

**kevy's value log is a corpus.** Records land in a vlog file in demotion
order, all from one keyspace, and the file is a contiguous sample of exactly
the population we want to model. That gives three things a per-datum
compressor structurally cannot have:

1. **Cross-value redundancy is visible.** A dictionary trained on a sample of
   the segment captures the repeated structure where the mass actually is.
2. **The values PG will not even try on are the ones we win.** Below 2 KB, PG
   does nothing; a dictionary makes small values the *best* case, not the
   worst. That is a category difference, not a percentage.
3. **A file inherits its predecessor's model.** Vlog files rotate within one
   keyspace, so the outgoing file's dictionary is already a trained model of
   the incoming file's population — the corpus is continuous across the
   rotation boundary even though the files are not (§7.2).

That is the ceiling-first claim: **on small structured values our ceiling is
structurally above PG's**, and it comes from the tiering model we already have
rather than from a better codec.

## 3. The other model fit: compaction is a free recompression point

`Vlog::compact_below` already scans a file, asks the owner which records are
still live, and rewrites the survivors. **It is already touching exactly the
bytes we would want to re-encode, in the background, off every hot path.**

So the ratio/latency trade need not be one global setting. It can be a
function of age, which is what the hot/cold model already believes in:

| stage | when | codec | why |
|---|---|---|---|
| **demote** | value leaves RAM | fast LZ level, dictionary-assisted | on the demote batch path; must not slow spill |
| **compact** | a file's live ratio falls below 0.5 | high-ratio level (entropy-coded) | already rewriting the bytes; the value has proven cold by surviving |

Cold has degrees, and compaction is where a value earns the more expensive
encoding by having stayed cold. No new background machinery is introduced —
the recompression rides a scan that already exists and already has a budget.

## 4. Prior art — the shoulders, and what each is here for

### 4.1 `spg_crypto::lzss` — the starting point, and why it is also the target

`~/workspace/goliajp/spg/crates/spg-crypto/src/lzss.rs`, 356 LOC, `no_std`,
zero deps, in production for spg's WAL and segment compression with an e2e gate
asserting ≥ 20 % smaller. Documented at ~2× on text, **~50 MiB/s encode,
~100 MiB/s decode**.

Reading it shows exactly where the speed goes, which makes "do better" concrete
rather than aspirational:

| what it does | consequence | what we do instead |
|---|---|---|
| `find_longest_match` walks **every** position in the window byte-by-byte (`while probe < pos`) | O(window × lookahead) per token — this is the 50 MiB/s | hash table of 4-byte sequences → one probe, LZ4-style |
| 12-bit offset → **4 KiB** window | matches beyond 4 KiB invisible | 16-bit offset → 64 KiB, and the dictionary extends the reachable history further still |
| 4-bit length → max match **18 bytes** | long runs cost many tokens | token nibbles + continuation bytes (LZ4), lengths unbounded |
| decoder copies **one byte at a time** | this is the 100 MiB/s | 8-byte wildcopy into an over-allocated output |
| per-buffer, no shared history | cross-value redundancy invisible | §2 — the whole point |

None of that is a criticism of spg's choice: a 356-LOC brute-force LZSS is the
right call for WAL frames where the gate is "20 % smaller" and throughput is
not the constraint. It is simply not the shape kevy's cold-read budget wants.

### 4.2 The published designs we take from

| source | what we take |
|---|---|
| **LZ4** (Collet) | the format that makes decode nearly memcpy-speed: token nibbles, wildcopy, single-probe hash match finder. This is the fast level |
| **zstd** (Collet) | dictionary mode — the mechanism for §2 — and FSE/Huffman entropy coding for the compaction level |
| **Snappy** (Google) | the incompressible early-abort discipline: cheap detection, store raw, never expand |
| **Brotli** | static-dictionary seeding, the closest published analogue to §2.3's declare-time seed |
| **spg lzss** | a working zero-dep LZ in the family, and its measured limits above |

## 5. Design sketch

A stone crate `kevy-compress`: `no_std`-friendly, zero deps, `forbid(unsafe)`
unless the wildcopy decoder demands otherwise (in which case `unsafe` is
confined to one reviewed function, as `kevy-vlog`'s CRC mirror already is).

```
encode(level, dict, input) -> Frame        // Frame = [tag][orig_len][payload]
decode(dict, frame)        -> Vec<u8>
```

Placement — everything is on paths that already exist:

- **demote batch** → `Vlog::append` receives an already-encoded body. The vlog
  record layout (`[body_len][crc32c][key_len][key][body]`) is unchanged; the
  frame tag lives inside the body, so **on-disk framing does not move** and
  `verify_image`'s CRC still covers exactly the bytes stored.
- **`compact_below`** → decode + re-encode at the high level while rewriting.
- **cold read** → `read_at` returns the frame; decode before handing back.

**The hot path is not touched at all.** No `SET` calls the encoder. That is a
structural property, not a tuning choice, and it is what makes the C4
non-regression obligation (KV and pubsub must not regress) provable by
inspection rather than only by benchmark.

### 5.1 Dictionary lifecycle rides the disposability we already have

The vlog is **disposable by design** — rebuilt each boot, AOF is the only
durability truth. A dictionary is therefore also disposable, which removes the
hardest problem this design would otherwise have: **no dictionary format
compatibility burden, ever.** A dictionary is built for a file, referenced by
that file, and dies with it.

Consequences to design against, not around:

- **Compaction crosses files.** Moving a record into a new file with a
  different dictionary means decode-then-re-encode — which §3 already does
  deliberately, so the cases coincide rather than conflict.
- **Training cost.** Sample-based training runs once per file (256 MiB
  rotation), amortised over thousands of records — and a new file starts from
  its predecessor's dictionary (§7.2), so there is no cold-start window in
  which records are encoded against an empty model.
- **A file must be self-describing.** The dictionary is stored in the file it
  serves, so a pinned `Arc<VlogFile>` still reads correctly during compaction —
  the pin semantics from the capacity arc carry over unchanged.

### 5.2 What stays uncompressed, and why that is architecture not laziness

**Hot values stay raw.** Compressing values in RAM would cut memory too — but
the hot/cold window *is already* the mechanism for trading RAM against CPU.
Adding a second, overlapping mechanism for the same trade makes both harder to
reason about and neither one authoritative. One mechanism per trade: the window
decides what is resident, compression decides what residency costs on disk.

Index structures are likewise out — an index hot/cold window is its own item in
the arc.

## 6. Acceptance criteria (gate-carried)

| # | criterion | carrier |
|---|---|---|
| **K1** | cold read p99 stays inside the existing B2 budget with compression on — decode must be memcpy-class, not lzss-class | capacity-envelope B2 |
| **K2** | **never expands**: encoded ≤ raw + frame header, for every input, including adversarial | fuzz |
| **K3** | round-trip identity under fuzz, including truncated/corrupt frames rejected rather than mis-decoded | fuzz (the `vlog_churn` target's shape) |
| **K4** | **the structural one** — N identical 400 B values in one segment encode to O(dictionary) + N × small, i.e. cross-value redundancy is actually captured. A per-datum baseline provably cannot pass this | kevy-compress integration test |
| **K5** | vlog amplification improves against B5's 1.27× on compressible corpora, and `compact_below` still terminates | capacity-envelope B5 / tiergate |
| **K6** | KV + pubsub perfgate lines unchanged, **plus** a static assertion that no encode call exists on the `SET` path | perfgate + a call-graph test |
| **K7** | disposability preserved: no dictionary state outside a vlog file, AOF remains the sole durability truth | tier_persistence B10/B11 |

**K4 is the one that says whether this design was worth doing** — it is the
claim from §2, and it is the claim PG structurally cannot match. K1 is the one
that can reject it: at spg's 100 MiB/s a 4 KB value costs ~40 µs to decode
against a 105 µs budget, so the fast level is a requirement of the design, not
a later optimisation.

### 6.1 What the ceiling is made of

Per §8.1 of the allocator RFC, state the residual rather than a target ratio.
After this design the bytes on disk are:

| term | what it is | scaling |
|---|---|---|
| **incompressible residual** | the corpus's actual entropy | O(true information) — irreducible, and the honest floor |
| **dictionary** | one per file, shared by all its records | O(files), not O(records) |
| **frame headers** | tag + original length per record | O(records), a few bytes each |
| **match-finder misses** | redundancy present but not found (window, hash collisions) | shrinks with encoder effort — the level knob |

Only the first is irreducible; the second is amortised by construction; the
third is bounded and tiny; the fourth is the only one a better encoder moves.
A headline ratio on real corpora falls out and goes in the docs — as a
consequence, not a goal.

## 7. Decisions (owner, 2026-07-26)

### 7.1 v1 is LZ-only plus dictionary; entropy coding is a named follow-up

§3's two levels remain the target architecture, but the compaction level
(FSE/Huffman) is roughly the same work again as the fast level, and **K4 — the
criterion that says whether this design was worth doing — does not depend on
it.** The category difference in §2 comes from the dictionary, not from entropy
coding. So v1 ships the fast level with dictionaries, and the compaction level
lands afterwards against the same K-criteria.

The two-stage structure in §3 is still built in v1: `compact_below` re-encodes,
it simply re-encodes at the same level for now. Adding the high-ratio level
later then changes one argument, not the design.

### 7.2 Seed from the previous file — not from the declaration

First, dispose of the wrong reason. "Seeding from a declaration would couple
the stone to the declaration layer" is **not** an argument against the
capability — layering settles it. The codec takes an optional caller-supplied
seed corpus (`&[u8]`); whoever assembles a vlog file passes whatever bytes it
likes, including field names rendered from a declaration, from a layer that
knows about declarations. **The seed is a parameter; it is never a
dependency.** kevy-compress goes on knowing only bytes, and the capability is
available whenever it is wanted. Fear of coupling must never cost a capability;
it only decides which layer the capability lives in.

The real question is therefore purely ceiling-first, and there the answer
changes: **a declaration is the weaker seed.** Declared field names are a
*subset* of what a sample finds — sampling catches the schema *and* the
repeated values: enum members, URL prefixes, shared timestamp prefixes, the
long tail of whatever this keyspace actually contains. A declaration cannot
know any of that. On any file large enough to matter, sampling strictly
dominates; a declaration only helps inside the cold-start window before
training has run.

And that window has a better answer hiding behind the question: **on rotation,
seed the new file's dictionary from the outgoing file's.** Same keyspace, same
population, already trained on real bytes. Cold start disappears by
construction, it costs nothing, and it works for undeclared keyspaces too —
strictly more general than anything a declaration could have offered.

So v1 trains from samples and seeds across rotation. The declaration-derived
seed remains reachable through the same parameter the moment it earns its
place.

## 8. Not in this RFC

Compressed hot values (§5.2) · index compression and an index hot/cold window ·
AOF compression (a durability-surface change; the vlog is disposable, the AOF
is not) · the auto-declaration loop · `kevy-alloc` (its own RFC).
