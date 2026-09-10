#!/usr/bin/env bash
# deadgate — the never-executed set may only shrink.
#
#   bash bench/deadgate.sh                    # gate against the baseline
#   bash bench/deadgate.sh --update-baseline  # record (refuses a worse set)
#
# covgate holds a percentage. This holds the identities behind it: coverage
# can sit at 79.64% for a year while the identity of the uncovered fifth is
# completely replaced, and the number never moves. Here every symbol that
# owns a never-executed region is named, and none may gain or join.
#
# The baseline is only ever recorded on the enforcing platform. Code
# switched off by cfg is ABSENT from a coverage run rather than dead in it —
# a macOS run sees 1 of 16 uring_*.rs files and none of kevy-uring — so a
# cross-platform comparison makes whole symbols leave the set, which a
# ratchet reads as improvement. setratchet's identity check refuses that
# comparison; this script does not need to remember it.
#
# Exit: 0 PASS/recorded, 1 the set grew, 2 refused.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
BASELINE="$HERE/DEAD-BASELINE.json"
OBSERVED="$HERE/DEAD-SET.json"
COV="${KEVY_COV_JSON:-$ROOT/target/llvm-cov-c1.json}"
# The corpus command, kept in one place. Changing it here without changing
# suite/corpus.toml is how two instruments start answering different
# questions while both look healthy.
CORPUS_ARGS="--workspace --exclude kevy-napi --lib --tests"
MODE=${1:-gate}

command -v cargo >/dev/null || { echo "deadgate: REFUSED — no cargo" >&2; exit 2; }

# The atlas verifies itself before it is trusted to measure anything. Symbol
# identity is what the whole ratchet holds, and it was computed by a regex
# that silently collapsed `<Type as Trait>::method` to `::method` — one
# identity absorbing every crate's `Debug`. The selftest carries a floor, so
# deleting the examples fails rather than passes quietly.
python3 "$ROOT/tools/coverage_atlas.py" --selftest || exit $?

if [ ! -f "$COV" ]; then
  echo "deadgate: producing the corpus run (this is the slow part)"
  # shellcheck disable=SC2086
  KEVY_TEST_PATIENCE=6 cargo llvm-cov $CORPUS_ARGS \
      --json --output-path "$COV" || {
    echo "deadgate: REFUSED — the corpus run failed; there is nothing to measure" >&2
    exit 2
  }
fi
[ -s "$COV" ] || { echo "deadgate: REFUSED — $COV is empty" >&2; exit 2; }

python3 "$ROOT/tools/coverage_atlas.py" "$COV" || exit $?

# Every unstable declaration must exempt something that exists.
#
# This check used to compare `suite/dead-paths.toml` against the `unstable`
# block inside DEAD-SET.json — and the atlas copies that block straight out
# of the same TOML, so it was comparing the register with itself. It said
# "register and this run agree" on every run it has ever made, and could
# not have said anything else.
#
# What it missed, found the day the symbol scheme changed: the register
# declared `kevy_geo::estimate_step`, and no symbol by that name has been in
# the set for some time — the real one is `kevy_geo::search::estimate_step`.
# A declaration that exempts nothing is a hole in the ratchet with a reason
# attached, which reads to the next person like a hole that was considered.
#
# So the comparison is now against the symbols actually observed. A stale
# declaration fails; so does an empty register, because a register that
# reads as empty is a broken read and not an absence of exemptions.
python3 - "$ROOT" <<'RECONCILE' || exit $?
import json, pathlib, sys, tomllib
root = pathlib.Path(sys.argv[1])
doc = tomllib.loads((root / "suite/dead-paths.toml").read_text())
reg = doc.get("unstable", [])
observed = json.loads((root / "bench/DEAD-SET.json").read_text()).get("symbols", {})
if not reg:
    print("deadgate: REFUSED — suite/dead-paths.toml declares no unstable "
          "entries; an empty register is a broken read, not agreement",
          file=sys.stderr)
    sys.exit(2)
if not observed:
    print("deadgate: REFUSED — the atlas observed no symbols at all; there is "
          "nothing for the register to be checked against", file=sys.stderr)
    sys.exit(2)
dead = []
for kind in ("unstable", "dead"):
    for e in doc.get(kind, []):
        if "symbol" in e:
            if e["symbol"] not in observed:
                dead.append(f"[[{kind}]] symbol {e['symbol']!r}")
        elif "prefix" in e:
            if not any(k.startswith(e["prefix"]) for k in observed):
                dead.append(f"[[{kind}]] prefix {e['prefix']!r}")
if dead:
    print("deadgate: FAIL — register entr(ies) naming nothing in this run")
    for x in dead:
        print(f"  {x}")
    print("  An [[unstable]] one is a hole in the ratchet with a reason attached;")
    print("  a [[dead]] one is a written reason for a region that is not there.")
    print("  Either the symbol was renamed, or it left the set and the entry can go.")
    sys.exit(1)
n = len(reg) + len(doc.get("dead", []))
print(f"deadgate: {n} register entr(ies), each naming a symbol this run observed")
RECONCILE

if [ "$MODE" = "--update-baseline" ]; then
  shift
  exec python3 "$ROOT/tools/setratchet.py" update "$BASELINE" "$OBSERVED" "$@"
fi
[ -f "$BASELINE" ] || {
  echo "deadgate: REFUSED — no $BASELINE. Record one on the enforcing" >&2
  echo "  platform first: bash bench/deadgate.sh --update-baseline" >&2
  exit 2
}
exec python3 "$ROOT/tools/setratchet.py" gate "$BASELINE" "$OBSERVED"
