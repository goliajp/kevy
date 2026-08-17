#!/usr/bin/env bash
# cookbook_smoke — every kevy-cli line in docs/cookbook.md must run green.
#
# The cookbook's recipes ship one executable ```console/```bash block
# each. This gate boots a throwaway kevy, extracts every line that
# starts with `kevy-cli` from those blocks, retargets the port
# (6004 → 7341), and runs them in document order against the same
# server. One `-ERR` (non-zero kevy-cli exit) fails the gate — except
# lines tagged `# needs-external`, which name endpoints this box
# doesn't have (the old-RDS mirror in recipe 13) and are skipped.
# Transient `-INDEXBUILDING` replies are retried: index backfill is
# asynchronous by contract (docs/indexes.md).
#
# Usage: bash bench/cookbook_smoke.sh [kevy-binary [kevy-cli-binary]]
set -u
cd "$(dirname "$0")/.."
KBIN=${1:-target/release/kevy}
CLI=${2:-target/release/kevy-cli}
DOC=docs/cookbook.md
PORT=7341
DIR=$(mktemp -d /tmp/kevy-cookbook-XXXXXX)
SPID=""
trap 'kill $SPID 2>/dev/null; rm -rf "$DIR"' EXIT

[ -x "$KBIN" ] || { echo "cookbook-smoke: FAIL — no kevy binary at $KBIN (cargo build --release -p kevy)"; exit 1; }
[ -x "$CLI" ] || { echo "cookbook-smoke: FAIL — no kevy-cli binary at $CLI (cargo build --release -p kevy-cli)"; exit 1; }

# Recipes 11-14 exercise FEED.* — the throwaway server runs with the
# feed on, exactly as the cookbook's prose asks of a real deployment.
printf '[feed]\nenabled = true\n' > "$DIR/kevy.toml"

env KEVY_BIND=127.0.0.1 "$KBIN" --config "$DIR/kevy.toml" --threads 4 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SPID=$!

# Wait for the listener.
for _ in $(seq 1 50); do
    "$CLI" -p $PORT PING >/dev/null 2>&1 && break
    sleep 0.2
done
"$CLI" -p $PORT PING >/dev/null 2>&1 || { echo "cookbook-smoke: FAIL — server did not come up"; exit 1; }

# Extract executable lines: inside ```console/```bash fences, lines
# that start with `kevy-cli`. Heredoc bodies (recipe 4's WATCH/MULTI
# session) don't start with kevy-cli, so only the opener would match —
# skip heredoc openers too (a REPL session isn't a one-shot command).
LINES=$(awk '
    /^```(console|bash)/ { fence = 1; next }
    /^```/               { fence = 0; next }
    fence && /^kevy-cli/ { print }
' "$DOC")

total=0
skipped=0
while IFS= read -r line; do
    [ -n "$line" ] || continue
    case $line in
        *'# needs-external'*) skipped=$((skipped + 1)); echo "cookbook-smoke: skip — $line"; continue ;;
        *'<<'*)               skipped=$((skipped + 1)); echo "cookbook-smoke: skip — $line"; continue ;;
    esac
    run=${line//6004/7341}
    # Every occurrence, not just the leading one: a recipe may nest a
    # kevy-cli call in a $() substitution (the FEED recipes read the
    # shard generation that way), and a reader's PATH has kevy-cli
    # where this harness has a build product.
    run=${run//kevy-cli/$CLI}
    tries=0
    while :; do
        out=$(eval "$run" 2>&1)
        rc=$?
        [ $rc -eq 0 ] && break
        if [ $tries -lt 100 ] && printf '%s' "$out" | grep -q INDEXBUILDING; then
            tries=$((tries + 1))
            sleep 0.3
            continue
        fi
        echo "cookbook-smoke: FAIL — $line"
        printf '%s\n' "$out"
        exit 1
    done
    total=$((total + 1))
done <<EOF
$LINES
EOF

[ $total -gt 0 ] || { echo "cookbook-smoke: FAIL — extracted 0 kevy-cli lines from $DOC"; exit 1; }
echo "cookbook-smoke: PASS ($total commands, $skipped skipped)"
