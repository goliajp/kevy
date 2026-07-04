#!/bin/bash
# v2.10 on-ramp gate — RFC D6 clamps with the real CLI binary:
#
#   1. 1M-row round trip (5 types + TTL): export → import --strict →
#      PREFIX.DIGEST equal on both ends, counts equal.
#   2. Import throughput ≥ 200k cmd/s (pipelined, localhost).
#   3. kill -9 mid-import → --resume completes → digest still equal.
#   4. delete-prefix --rate accuracy within ±20%.
#
# Usage: bash bench/onrampgate.sh
set -u
cd "$(dirname "$0")/.."
cargo build --release -p kevy --bin kevy -p kevy-cli --bin kevy-cli 2>&1 | tail -1
KEVY=target/release/kevy
CLI=target/release/kevy-cli
PA=7071
PB=7072
DIR=$(mktemp -d /tmp/kevy-onramp-XXXXXX)

env KEVY_BIND=127.0.0.1 $KEVY --threads 8 --port $PA --dir "$DIR/a" --no-aof >/dev/null 2>&1 &
SA=$!
env KEVY_BIND=127.0.0.1 $KEVY --threads 8 --port $PB --dir "$DIR/b" --no-aof >/dev/null 2>&1 &
SB=$!
sleep 1.2
trap 'kill $SA $SB 2>/dev/null; rm -rf "$DIR"' EXIT

# ---- seed 1M rows on A (python loader, 5 types + TTL) ----
python3 - "$PA" <<'PYEOF'
import socket, sys, time
port = int(sys.argv[1])
def enc(*parts):
    buf = b"*%d\r\n" % len(parts)
    for p in parts:
        if isinstance(p, str):
            p = p.encode()
        buf += b"$%d\r\n%s\r\n" % (len(p), p)
    return buf
s = socket.create_connection(("127.0.0.1", port))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
buf = [b""]
def rd():
    while b"\r\n" not in buf[0]:
        buf[0] += s.recv(1 << 20)
    l, _, rest = buf[0].partition(b"\r\n")
    buf[0] = rest
    return l
t0 = time.time()
batch = []
def flush():
    global batch
    if batch:
        s.sendall(b"".join(batch))
        for _ in range(len(batch)):
            rd()
        batch = []
N = 940_000
for i in range(N):
    batch.append(enc("SET", f"mig:s:{i}", f"value-{i}"))
    if len(batch) == 2000:
        flush()
for i in range(20_000):
    batch.append(enc("HSET", f"mig:h:{i}", "a", str(i), "b", "x"))
    batch.append(enc("RPUSH", f"mig:l:{i}", "1", "2", "3"))
    if len(batch) >= 2000:
        flush()
for i in range(10_000):
    batch.append(enc("SADD", f"mig:t:{i}", "p", "q"))
    batch.append(enc("ZADD", f"mig:z:{i}", "1.5", "m", "2.5", "n"))
    if len(batch) >= 2000:
        flush()
batch.append(enc("SET", "mig:ttlkey", "soon"))
batch.append(enc("PEXPIRE", "mig:ttlkey", "3600000"))
flush()
print(f"onrampgate: seeded 1,000,001 keys in {time.time()-t0:.1f}s", flush=True)
PYEOF

# ---- clamp 1+2: export → import, throughput ----
T0=$(date +%s.%N)
$CLI export -p $PA --prefix mig: "$DIR/dump.resp" || exit 1
T1=$(date +%s.%N)
echo "onrampgate: export took $(echo "$T1 - $T0" | bc)s"
IMP_OUT=$($CLI import -p $PB --strict "$DIR/dump.resp") || exit 1
T2=$(date +%s.%N)
echo "onrampgate: $IMP_OUT"
CMDS=$(echo "$IMP_OUT" | grep -oE "[0-9]+ ok" | grep -oE "[0-9]+")
SECS=$(echo "$T2 - $T1" | bc)
RATE=$(echo "$CMDS / $SECS" | bc)
echo "onrampgate: import $CMDS cmds in ${SECS}s = ${RATE}/s"
if [ "$RATE" -lt 200000 ]; then
    echo "onrampgate: FAIL — import rate ${RATE}/s < 200k/s"
    exit 1
fi
DA=$($CLI digest -p $PA mig:)
DB=$($CLI digest -p $PB mig:)
echo "onrampgate: digest A=$DA B=$DB"
if [ "$DA" != "$DB" ]; then
    echo "onrampgate: FAIL — round-trip digest mismatch"
    exit 1
fi

# ---- clamp 3: kill -9 mid-import → resume ----
$CLI delete-prefix -p $PB mig: >/dev/null
rm -f "$DIR/dump.progress"
$CLI import -p $PB "$DIR/dump.resp" >/dev/null 2>&1 &
IMP=$!
sleep 1.2
kill -9 $IMP 2>/dev/null
wait $IMP 2>/dev/null
OFF=$(cat "$DIR/dump.progress" 2>/dev/null || echo 0)
echo "onrampgate: killed mid-import at offset $OFF"
if [ "$OFF" = "0" ]; then
    echo "onrampgate: FAIL — no progress recorded before kill"
    exit 1
fi
$CLI import -p $PB --resume --strict "$DIR/dump.resp" >/dev/null || exit 1
DB2=$($CLI digest -p $PB mig:)
if [ "$DA" != "$DB2" ]; then
    echo "onrampgate: FAIL — post-resume digest mismatch ($DA vs $DB2)"
    exit 1
fi
echo "onrampgate: resume after kill -9 converged"

# ---- clamp 4: rate accuracy ±20% (2000 keys @ 1000/s ≈ 2s) ----
T0=$(date +%s.%N)
$CLI delete-prefix -p $PB --rate 1000 mig:h: >/dev/null || exit 1
T1=$(date +%s.%N)
SECS=$(echo "$T1 - $T0" | bc)
echo "onrampgate: rated delete of 20k keys @1000/s took ${SECS}s (expect ~20s ±20%)"
OK=$(echo "$SECS >= 16 && $SECS <= 24" | bc)
if [ "$OK" != "1" ]; then
    echo "onrampgate: FAIL — rate accuracy off (${SECS}s for 20s of work)"
    exit 1
fi
echo "onrampgate: PASS"
