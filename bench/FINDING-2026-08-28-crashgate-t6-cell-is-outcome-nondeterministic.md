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
