#!/usr/bin/env bash
# crashgate — the crash-consistency contract, executed.
#
# SIGKILLs a live writer at hostile moments (mid-append burst, mid-rewrite,
# mid-snapshot, mid-feed-emit; everysec and always fsync; 1 and 4 shards),
# then asserts what docs/persistence.md §contract promises:
#
#   loss-bound     everything at or before the writer's last fsync barrier
#                  (its last `SYNCED n` line) survives the kill
#   no-blackhole   a post-crash restart's writes survive the NEXT restart —
#                  the replay stop point never regresses (the 3.18 black
#                  hole: append after a stuck corrupt frame, every restart
#                  rolled back and re-lost the interim writes)
#   quarantine     a corrupt tail is copied aside before truncation
#                  [REDpending(T2) until the quarantine train lands]
#   recovery-rate  a corrupt frame MID-file loses only the bad frame, not
#                  the good tail behind it [REDpending(T6) until resync]
#
# Gate-first discipline (durability-trust RFC T1): the pending cells are
# EXPECTED red today and are printed as `REDpending(<train>)`; the arc
# cannot finish until every line is PASS. Exit status: 0 iff no
# unexpected FAIL (pending reds don't fail the build until their train
# lands — flip PENDING_STRICT=1 at arc finish).
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
STRICT=${PENDING_STRICT:-0}

cargo build -q -p kevy-embedded --example crash_writer --example crash_check \
    --example crash_window_writer --example crash_window_check || exit 1
WRITER="$HERE/target/debug/examples/crash_writer"
CHECK="$HERE/target/debug/examples/crash_check"
WWRITER="$HERE/target/debug/examples/crash_window_writer"
WCHECK="$HERE/target/debug/examples/crash_window_check"

WORK=$(mktemp -d /tmp/crashgate.XXXXXX)
WPID=""
cleanup() { [ -n "$WPID" ] && kill -9 "$WPID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

fail=0
verdict() { # verdict <ok:0/1> <label> <detail>
    if [ "$1" -eq 0 ]; then echo "crashgate: PASS  $2 ($3)"; else echo "crashgate: FAIL  $2 ($3)"; fail=1; fi
}
pending() { # pending <ok:0/1> <train> <label> <detail>
    if [ "$1" -eq 0 ]; then echo "crashgate: PASS  $3 ($4)"; else
        echo "crashgate: REDpending($2)  $3 ($4)"
        [ "$STRICT" = "1" ] && fail=1
    fi
}

# One kill cycle: start writer with $flags, kill it mid-flight, then run the
# loss-bound + no-blackhole probes. $1 = cell name, rest = writer flags.
cell() {
    local name=$1; shift
    local flags=("$@") dir="$WORK/$name" log="$WORK/$name.log"
    mkdir -p "$dir"
    "$WRITER" "$dir" ${flags[@]+"${flags[@]}"} > "$log" 2>/dev/null &
    WPID=$!
    # Let it write through at least a few fsync barriers, then murder it.
    for _ in $(seq 100); do grep -q "SYNCED" "$log" 2>/dev/null && break; sleep 0.1; done
    sleep 1.2
    kill -9 "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null; WPID=""
    local synced
    synced=$(grep SYNCED "$log" | tail -1 | awk '{print $2}')
    [ -n "$synced" ] || { verdict 1 "$name/setup" "writer never reached a SYNCED barrier"; return; }

    # Reopen #1: loss bound, then mark 100 clean writes.
    local checkflags=() fl=" ${flags[*]+${flags[*]}} "
    [[ "$fl" == *" --shards "* ]] && checkflags+=(--shards "$(echo "$fl" | sed 's/.*--shards \([0-9]*\).*/\1/')")
    [[ "$fl" == *" --feed "* ]] && checkflags+=(--feed)
    local out1 rec1 marked
    out1=$("$CHECK" "$dir" ${checkflags[@]+"${checkflags[@]}"} --mark 2>/dev/null)
    rec1=$(echo "$out1" | awk '/^RECOVERED/{print $2}')
    marked=$(echo "$out1" | awk '/^MARKED/{print $2}')
    [ "${rec1:-0}" -ge "$synced" ] && verdict 0 "$name/loss-bound" "recovered $rec1 >= synced $synced" \
        || verdict 1 "$name/loss-bound" "recovered ${rec1:-0} < synced $synced"

    # Reopen #2: the post-crash marks must survive the next restart.
    local rec2
    rec2=$("$CHECK" "$dir" ${checkflags[@]+"${checkflags[@]}"} 2>/dev/null | awk '/^RECOVERED/{print $2}')
    [ "${rec2:-0}" -ge "${marked:-1}" ] && verdict 0 "$name/no-blackhole" "restart#2 sees $rec2 >= marked $marked" \
        || verdict 1 "$name/no-blackhole" "restart#2 sees ${rec2:-0} < marked ${marked:-?} — restarts are rolling back"
}

# One windowed kill cycle: a fast-reaper store slides continuously
# (seal, manifest, SEGMENTED frame, phase change, index cut, text
# freeze), the kill lands in whichever gap it lands in, and the reopen
# must hold the R2c contract: loss bound across BOTH truth homes (hot
# + row segments), index/row agreement, a serving text index — then a
# second reopen must not roll any of it back.
window_cell() { # $1 = cell name, rest = writer flags
    local name=$1; shift
    local flags=("$@") dir="$WORK/$name" log="$WORK/$name.log"
    mkdir -p "$dir"
    "$WWRITER" "$dir" ${flags[@]+"${flags[@]}"} > "$log" 2>/dev/null &
    WPID=$!
    for _ in $(seq 100); do grep -q "SYNCED" "$log" 2>/dev/null && break; sleep 0.1; done
    sleep 1.5
    kill -9 "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null; WPID=""
    local synced
    synced=$(grep SYNCED "$log" | tail -1 | awk '{print $2}')
    [ -n "$synced" ] || { verdict 1 "$name/setup" "writer never reached a SYNCED barrier"; return; }
    local checkflags=() fl=" ${flags[*]+${flags[*]}} "
    [[ "$fl" == *" --shards "* ]] && checkflags+=(--shards "$(echo "$fl" | sed 's/.*--shards \([0-9]*\).*/\1/')")
    local out1 rc count1
    out1=$("$WCHECK" "$dir" "$synced" ${checkflags[@]+"${checkflags[@]}"} 2>/dev/null); rc=$?
    count1=$(echo "$out1" | awk -F'count=' '/^OK/{print $2}' | awk '{print $1}')
    [ "$rc" -eq 0 ] && verdict 0 "$name/contract" "$out1" \
        || verdict 1 "$name/contract" "reopen violated the window contract (synced $synced)"
    # Reopen #2: nothing may roll back (segments re-swept, not re-lost).
    local count2
    count2=$("$WCHECK" "$dir" "$synced" ${checkflags[@]+"${checkflags[@]}"} 2>/dev/null \
        | awk -F'count=' '/^OK/{print $2}' | awk '{print $1}')
    [ -n "$count2" ] && [ "${count2:-0}" -ge "${count1:-1}" ] \
        && verdict 0 "$name/no-blackhole" "restart#2 counts $count2 >= $count1" \
        || verdict 1 "$name/no-blackhole" "restart#2 counts ${count2:-none} < ${count1:-?}"
}

echo "== crashgate: SIGKILL matrix =="
cell append-everysec
cell append-always --always
cell append-4shard --shards 4
cell rewrite-everysec --rewrite
cell snapshot-everysec --snapshot
cell feed-everysec --feed

echo "== crashgate: windowed SIGKILL cells (R2c) =="
window_cell window-everysec-a
window_cell window-everysec-b
window_cell window-always --always
window_cell window-2shard --shards 2

# ── deterministic damage cells ─────────────────────────────────────────────
# The kill cells rarely tear a frame on a journaled local fs (page-cache
# writes of a small buffer land atomically), so damage is injected by hand —
# the same shapes the mailrs incident and a real torn write leave behind.

echo "== crashgate: injected-damage cells =="

# G — torn tail: cut the last frame mid-byte. Open must repair (truncate),
# keep the loss bound, not black-hole — and (T2) quarantine the cut bytes.
dir="$WORK/torn"; log="$WORK/torn.log"; mkdir -p "$dir"
"$WRITER" "$dir" > "$log" 2>/dev/null & WPID=$!
for _ in $(seq 100); do grep -q "SYNCED" "$log" 2>/dev/null && break; sleep 0.1; done
sleep 0.8; kill -9 "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null; WPID=""
synced=$(grep SYNCED "$log" | tail -1 | awk '{print $2}')
aof="$dir/aof-0.aof"; size=$(stat -f%z "$aof" 2>/dev/null || stat -c%s "$aof")
truncate -s $((size - 7)) "$aof" # torn: last frame loses its tail bytes
out1=$("$CHECK" "$dir" --mark 2>/dev/null); rc=$?
rec1=$(echo "$out1" | awk '/^RECOVERED/{print $2}'); marked=$(echo "$out1" | awk '/^MARKED/{print $2}')
q1=$(echo "$out1" | awk '/^QUARANTINE/{print $2}')
verdict $rc "torn-tail/open" "open after torn tail rc=$rc"
# The 7-byte cut destroys at most ONE acknowledged frame (possibly the very
# frame the last fsync barrier covered), so the bound here is synced-1 — the
# injection ate that frame, not kevy.
[ "${rec1:-0}" -ge "$((synced - 1))" ] && verdict 0 "torn-tail/loss-bound" "recovered $rec1 >= synced-1 ($synced-1)" \
    || verdict 1 "torn-tail/loss-bound" "recovered ${rec1:-0} < synced-1 ($synced-1)"
rec2=$("$CHECK" "$dir" 2>/dev/null | awk '/^RECOVERED/{print $2}')
[ "${rec2:-0}" -ge "${marked:-1}" ] && verdict 0 "torn-tail/no-blackhole" "restart#2 sees $rec2 >= marked $marked" \
    || verdict 1 "torn-tail/no-blackhole" "restart#2 sees ${rec2:-0} < marked ${marked:-?}"
[ "${q1:-0}" -ge 1 ] && pending 0 T2 "torn-tail/quarantine" "$q1 quarantine file(s)" \
    || pending 1 T2 "torn-tail/quarantine" "torn bytes were destroyed, not set aside"

# H — corrupt frame MID-file (the mailrs shape): SPLICE 7 bytes out at ~50%
# depth, so the parser derails there with megabytes of GOOD frames behind
# it (a byte FLIP lands inside a bulk payload and never breaks framing —
# that case is cell I below). Open must still succeed and not black-hole;
# T2 wants the dropped region quarantined; T6 wants the good tail behind
# the bad frame RECOVERED.
dir="$WORK/midfile"; log="$WORK/midfile.log"; mkdir -p "$dir"
"$WRITER" "$dir" > "$log" 2>/dev/null & WPID=$!
for _ in $(seq 100); do grep -q "SYNCED" "$log" 2>/dev/null && break; sleep 0.1; done
sleep 1.2; kill -9 "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null; WPID=""
synced=$(grep SYNCED "$log" | tail -1 | awk '{print $2}')
aof="$dir/aof-0.aof"; size=$(stat -f%z "$aof" 2>/dev/null || stat -c%s "$aof")
mid=$((size / 2))
head -c "$mid" "$aof" > "$aof.spliced"
tail -c "+$((mid + 8))" "$aof" >> "$aof.spliced"
mv "$aof.spliced" "$aof"
# Resync probe FIRST: the strict open below repairs (quarantines +
# truncates) the tail, destroying exactly what resync exists to recover.
recr=$("$CHECK" "$dir" --resync 2>/dev/null | awk '/^RECOVERED/{print $2}')
out1=$("$CHECK" "$dir" --mark 2>/dev/null); rc=$?
rec1=$(echo "$out1" | awk '/^RECOVERED/{print $2}'); marked=$(echo "$out1" | awk '/^MARKED/{print $2}')
q1=$(echo "$out1" | awk '/^QUARANTINE/{print $2}')
verdict $rc "midfile-corrupt/open" "open after mid-file corruption rc=$rc"
rec2=$("$CHECK" "$dir" 2>/dev/null | awk '/^RECOVERED/{print $2}')
[ "${rec2:-0}" -ge "${marked:-1}" ] && verdict 0 "midfile-corrupt/no-blackhole" "restart#2 sees $rec2 >= marked $marked" \
    || verdict 1 "midfile-corrupt/no-blackhole" "restart#2 sees ${rec2:-0} < marked ${marked:-?}"
[ "${q1:-0}" -ge 1 ] && pending 0 T2 "midfile-corrupt/quarantine" "$q1 quarantine file(s)" \
    || pending 1 T2 "midfile-corrupt/quarantine" "231MB-class good tail destroyed, not set aside"
[ "${recr:-0}" -ge "${synced:-1}" ] && pending 0 T6 "midfile-corrupt/recovery-rate" "resync recovered the good tail ($recr >= $synced)" \
    || pending 1 T6 "midfile-corrupt/recovery-rate" "resync recovered ${recr:-0} < synced $synced"

# I — payload byte-flip: framing stays intact, a VALUE is silently corrupted.
# Discovered by this gate's first run: a flip inside a bulk payload replays
# without any complaint — there is no integrity check on today's format.
# T5's frame CRC exists to catch exactly this. Deterministic injection: flip
# the middle of a known 0x62-run (crash_writer's values are uniform 'b').
dir="$WORK/taint"; log="$WORK/taint.log"; mkdir -p "$dir"
"$WRITER" "$dir" > "$log" 2>/dev/null & WPID=$!
for _ in $(seq 100); do grep -q "SYNCED" "$log" 2>/dev/null && break; sleep 0.1; done
sleep 0.5; kill -9 "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null; WPID=""
aof="$dir/aof-0.aof"
# The LAST 'b'-run: the newest version of whichever k<i> wrote last — no
# later frame overwrites it, so the taint must survive replay (flipping an
# EARLY run gets silently healed by a later version of the same key: this
# gate's own first run made that mistake and spuriously passed).
off=$(grep -abo 'bbbbbbbbbbbbbbbb' "$aof" | tail -1 | cut -d: -f1)
if [ -z "$off" ]; then
    verdict 1 "payload-flip/setup" "no payload run found to corrupt"
else
    printf '\xff' | dd of="$aof" bs=1 seek=$((off + 8)) count=1 conv=notrunc 2>/dev/null
    out1=$("$CHECK" "$dir" 2>/dev/null); rc=$?
    tainted=$(echo "$out1" | awk '/^TAINTED/{print $2}')
    verdict $rc "payload-flip/open" "open after payload flip rc=$rc"
    [ "${tainted:-1}" -eq 0 ] && pending 0 T5 "payload-flip/integrity" "corrupt value detected, not replayed" \
        || pending 1 T5 "payload-flip/integrity" "$tainted tainted value(s) replayed silently — no on-disk integrity check"
fi

echo
[ "$fail" -eq 0 ] && echo "crashgate: PASS (pending cells listed above gate the arc finish)" || echo "crashgate: FAIL"
exit "$fail"
