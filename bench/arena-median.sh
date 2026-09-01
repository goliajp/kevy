#!/usr/bin/env bash
# arena-median — N full arena runs, per-cell medians, one ledger table.
#
# Why this exists: `perfgate-median` already says that a single run cannot
# green or red an angle inside the instrument's own band, and it wraps
# `perfgate` — the gate that measures kevy against a reference commit of
# ITSELF. The table that goes in front of readers is `arena`'s, and it had
# no multi-run variant. The noise-resistant instrument was on the private
# ratchet and not on the public claim.
#
# What made that concrete: on 2026-09-01 three arena runs of one unchanged
# binary disagreed by 26.6% on SADD and 13% on INCR, while valkey's worst
# cell moved 8.6% and Redis 8's 7.6% across the same three runs. Within-run
# stdev over five iterations does not predict that; only more runs do.
#
#   bash bench/arena-median.sh <KEVY_BIN> [N]
#
# Prints the ledger-shaped table on per-cell medians, the run-to-run spread
# beside each engine, and the claim medians cannot make on their own:
# whether kevy's WORST run still beats each competitor's BEST run.
#
# Exit: 0 = kevy's worst beats every competitor's best in every cell;
# 1 = at least one cell needs the median to win (say so in the ledger
# rather than quietly reporting the median); 2 = a run produced nothing.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:?usage: arena-median.sh <KEVY_BIN> [N]}
N=${2:-3}
OUT=$(mktemp -d "${TMPDIR:-/tmp}/armed-XXXXXX")
trap 'rm -rf "$OUT"' EXIT

for i in $(seq 1 "$N"); do
    echo "arena-median: run $i/$N" >&2
    if ! bash "$HERE/arena.sh" "$BIN" >"$OUT/run$i" 2>"$OUT/err$i"; then
        echo "arena-median: run $i failed" >&2
        tail -3 "$OUT/err$i" >&2
        exit 2
    fi
    # arena emits `<engine> <verb> <median> <stdev>`; the run number rides
    # along so the spread can be computed per cell.
    awk -v r="$i" '/^(kevy|valkey|redis8|dragonfly) /{print $1, $2, $3, r}' \
        "$OUT/run$i" >>"$OUT/samples"
done
[ -s "$OUT/samples" ] || { echo "arena-median: no samples parsed from $N runs" >&2; exit 2; }

python3 - "$OUT/samples" "$N" <<'PY'
import statistics, sys
rows = {}
for line in open(sys.argv[1]):
    engine, verb, val, _run = line.split()
    rows.setdefault((engine, verb), []).append(int(val))
n = int(sys.argv[2])
verbs = ["GET", "SET", "INCR", "SADD", "HSET", "LPUSH", "ZADD"]
engines = ["kevy", "redis8", "valkey", "dragonfly"]
label = {"kevy": "kevy", "redis8": "Redis 8", "valkey": "valkey", "dragonfly": "Dragonfly"}

med = {k: statistics.median(v) for k, v in rows.items()}
print(f"# arena-median over {n} runs — per-cell medians\n")
print("| verb | " + " | ".join(label[e] for e in engines) + " | vs Redis 8 |")
print("|---|" + "---:|" * (len(engines) + 1))
for v in verbs:
    cells = " | ".join(f"{med[(e, v)]:,.0f}" for e in engines)
    print(f"| {v} | {cells} | {med[('kevy', v)] / med[('redis8', v)]:.2f}x |")

print(f"\n## Run-to-run spread over {n} runs\n")
print("| engine | worst cell | spread |")
print("|---|---|---:|")
for e in engines:
    worst, val = None, -1.0
    for v in verbs:
        s = rows[(e, v)]
        pct = (max(s) - min(s)) / statistics.median(s) * 100
        if pct > val:
            worst, val = v, pct
    print(f"| {label[e]} | {worst} | {val:.1f}% |")

print("\n## kevy's worst run against each competitor's best\n")
print("| verb | " + " | ".join(f"vs {label[e]}" for e in engines[1:]) + " |")
print("|---|" + "---:|" * (len(engines) - 1))
weak = []
for v in verbs:
    kmin = min(rows[("kevy", v)])
    cells = []
    for e in engines[1:]:
        r = kmin / max(rows[(e, v)])
        cells.append(f"{r:.2f}x")
        if r <= 1.0:
            weak.append(f"{v} vs {label[e]} ({r:.2f}x)")
    print(f"| {v} | " + " | ".join(cells) + " |")

if weak:
    print("\n**Needs the median to win:** " + ", ".join(weak) + ".")
    print("State that in the ledger entry rather than reporting only the median.")
    sys.exit(1)
print("\n**kevy's worst run beats every competitor's best run, in every cell.**")
PY
