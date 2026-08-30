# FINDING 2026-08-28 — crashgate's T6 recovery-rate cell is outcome-nondeterministic under PENDING_STRICT

**Status**: OPEN, pre-existing, and not caused by the change it first failed
on. Blocking CI intermittently.

## What happened

`crashgate`'s `midfile-corrupt/recovery-rate` cell failed on run
33096325757 and passed on the three runs around it:

| run | commit | recovered | synced | verdict |
|---|---|---:|---:|---|
| 33093335244 | 8b9f6134 | 156,000 | 156,000 | PASS |
| 33094996325 | 3507540f | 184,000 | 184,000 | PASS |
| **33096325757** | **13804239** | **85,121** | **170,000** | **REDpending(T6)** |

CI runs the gate with `PENDING_STRICT=1`, so a pending red fails the build.

## It is not the commit it failed on

13804239 changed `suite/dead-paths.toml`, two Python tools and two JSON
baselines. Not one line of engine code, and crashgate exercises AOF recovery
in a real server. A cause has to be able to reach the effect, and this one
cannot.

## Why the outcome varies

The cell writes for ~1.2 s, kills the writer, then splices seven bytes out
at **`size / 2`** — a byte offset, not a frame boundary:

```sh
mid=$((size / 2))
head -c "$mid" "$aof" > "$aof.spliced"
tail -c "+$((mid + 8))" "$aof" >> "$aof.spliced"
```

Where that lands inside a frame decides whether the resync scanner can find
the next frame header behind the damage. When it can, recovery is total —
`recr == synced` exactly, which is what both passing runs show. When it
cannot, recovery stops at the splice, which is what 85,121 of 170,000 is:
50.07%, the first half and nothing after.

Both the file size (how much a 1.2-second writer produced on that runner)
and the payload sizes that set the framing vary run to run. So does the
outcome.

## What this is not

It is not a boundary flake — 50.07% is not a near-miss on a threshold, it is
the difference between resync working and resync not engaging at all. The
cell is measuring a real property; it is the *input* that is not held fixed.

## What would fix it, and what would only hide it

**Would fix it**: splice at a deterministic point relative to the frame
structure — the same nth frame every run — so the cell tests "damage
mid-frame with good frames behind it" without also rolling dice on whether
resync has a header to find. That preserves what T6 is about.

**Would only hide it**: disarming `PENDING_STRICT`, or loosening the
threshold from `>= synced` to a fraction. Both would let the cell pass on
runs where the good tail really was lost, which is the exact failure T6
exists to close.

This is not fixed here. It is someone else's gate, the fix changes what the
cell tests, and choosing that is a decision about the durability arc rather
than a repair — but the three numbers above are what any such decision
should start from.

---

## Continuation, same day: eight more runs, and one hypothesis refuted

Eight consecutive crashgate runs on 6.0.0 (lx64, release binary), reading
only the T6 cell:

```
228500 >= 228500    225500 >= 225500
231000 >= 231000    225000 >= 225000
227500 >= 227500    226000 >= 226000
216000 >= 216000    206803 >= 206500
```

All eight pass. Seven land on exact equality; one recovers **more** than was
acknowledged, which is resync picking up writes that were in flight when the
writer died — allowed, and a useful reminder that `recr >= synced` is a
lower bound rather than an identity.

So the failure rate on this box is around one in eleven observed, not one in
three. That does not weaken the finding: 50.07% is still the difference
between resync engaging and not engaging, and a cell that answers that
question on roughly ten runs in eleven is not answering it deliberately.

**A hypothesis worth recording because it was wrong.** Before running these,
the guess was that `size / 2` usually lands in data the writer had NOT yet
synced, so the destroyed bytes would usually cost nothing and the cell would
mostly not be testing its own question. Measured directly — writer run,
killed, then the synced record count converted to a byte offset through the
file's own bytes-per-record:

| run | synced records | file bytes | splice point | synced prefix ends ≈ | splice inside synced |
|---|---:|---:|---:|---:|---|
| 1 | 67,500 | 22,661,425 | 11,330,712 | 22,612,500 | yes |
| 2 | 71,000 | 24,004,985 | 12,002,492 | 23,785,000 | yes |
| 3 | 78,000 | 26,188,323 | 13,094,161 | 26,130,000 | yes |

The synced prefix covers essentially the whole file, so the splice is always
inside acknowledged data. The cell **is** asking its question; what varies is
only what the original finding said varies — which byte inside a frame the
cut lands on. The fix it proposes is unchanged, and now it is the only
explanation left standing.

---

## Resolution, same day: the cell was right, the engine was wrong

The instrument added earlier — keeping the `--resync` stderr this gate had
been sending to `/dev/null` — spoke on the next CI failure:

```
crashgate: T6 replay said (splice at 27485183 of 54970366 bytes):
    kevy: AOF …/midfile/aof-0.aof replayed 163722 commands from 54970359
    bytes in 1598 ms; trailing 27485178 bytes were a partial frame
    (crash mid-append, recoverable)
```

No corrupt WARN: **resync never ran**. And 27,485,178 bytes called a partial
frame from a crash mid-append.

An AOF record header is `len: u32 LE` + `crc: u32 LE`. After the splice the
next header is read from what used to be payload, and there are three ways
out — two of which recover:

| the bytes read as | walk's verdict | resync | outcome |
|---|---|---|---|
| `len == 0` or `> MAX_RECORD` | CorruptFrame | ran | tail recovered |
| `len` legal, larger than what remains | **TruncatedTail** | **never ran** | **tail lost** |
| `len` fits, CRC disagrees | CorruptFrame | ran | tail recovered |

That is the coin, and it explains everything this finding recorded: 50.07%
and 50.22% are the splice offset, one in five on CI, none in 28 bench-box
runs or 65 splice positions because those files never produced the middle
reading.

**The cell was doing its job.** It is not the input that needed fixing.

Reproduced deterministically rather than waited for — ten good records, one
record whose length claims a megabyte with five bytes behind it, ten more —
and `replay_aof_resync` returned 10 of 20 with zero ranges reported. Its
sibling, where the CRC lies instead, has always returned all 20. Same damage,
same position, opposite outcome, decided by which lie the bytes told.

Three parts, all fixed and each with its own test or branch:

1. resync now runs on any stop that is not clean — the question was always
   "is there anything valid after where we stopped".
2. `corrupt` is raised by a skipped range, not only by the stop reason. It
   had recovered the tail and still reported the file healthy, against
   `docs/persistence.md`'s stated contract.
3. The default-path message no longer calls a multi-megabyte drop a partial
   frame; the walk already knew a short HEADER read from a short PAYLOAD read
   and gave both the same verdict.

## Settled, 2026-08-30: twenty-eight runs

Two greens were not enough to tell the fix from luck, and the declaration
stayed until more runs answered it. They have.

**Twenty-eight consecutive CI runs pass the T6 cell**, every one of them on
a commit containing the resync fix, all on 2026-08-30. (A twenty-ninth was
cancelled by a superseding push, which is not a reading either way.)

Against the failure rate this finding recorded on CI — one in five — twenty
-eight consecutive passes have probability **0.0019: about one in five
hundred**. Against the gentler rate the bench box showed, one in eleven, it
is 0.07, or one in fourteen. The CI rate is the relevant one: the failures
were CI failures and these are CI runs.

The cell is a hard `verdict`, not a `pending` — `bench/crashgate.sh:232`
and `:234` — so those twenty-eight are the gate passing, not a pending red
being tolerated. The header comment already said as much; this is the
evidence behind it.

**T6 is closed.** The mechanism was closed by construction on the day it was
found; this is the part that had to be waited for rather than argued.
