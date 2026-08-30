#!/usr/bin/env bash
# v3.9-t1 onramp drill — a mailrs-SHAPED migration rehearsal, end to
# end, beyond onrampgate's four clamps. Purpose: walk the exact road
# a real app migration walks and record every UX friction point:
#
#   1. seed a mail-shaped keyspace (big hash bodies, zset mailboxes,
#      TTL'd sessions — mixed types across five prefixes)
#   2. export (timed) → import --strict into a fresh server (timed)
#   3. digest/diff per prefix, both ends
#   4. kill -9 mid-import → --resume → converge → diff again
#   5. post-load IDX.CREATE (range + TEXT over the big bodies) →
#      watch backfill-readiness UX
#   6. copy-prefix --rate / delete-prefix round trip
#
# Output: PASS/FAIL per step + timings; frictions land in the
# accompanying finding doc. Not a gate — a rehearsal harness.
set -u
cd "$(dirname "$0")/.."
cargo build --release -p kevy --bin kevy -p kevy-cli --bin kevy-cli 2>&1 | tail -1
KEVY=target/release/kevy
CLI=target/release/kevy-cli
SRC=7301
DST=7302
DIR=$(mktemp -d /tmp/kevy-drill-XXXXXX)
SPID=""
DPID=""
trap 'kill $SPID $DPID 2>/dev/null; rm -rf "$DIR"' EXIT

# Wait until a port has no listener at all. kevy binds one socket per shard
# with SO_REUSEPORT, so a new server can bind ALONGSIDE a dying one and the
# kernel will hand connections to both. availgate has carried this guard for
# the same reason; this drill did not.
wait_port_free() {
    local port=$1 t0=$SECONDS
    while ss -ltn 2>/dev/null | grep -q ":$port "; do
        [ $((SECONDS - t0)) -ge 30 ] && { echo "drill: port $port never freed" >&2; exit 1; }
        sleep 0.1
    done
}

# Stop a server and WAIT for it to be gone. `wait $pid` cannot do this: the
# server is started inside a $( ) subshell, so it is not a child of this
# shell and `wait` returns immediately having waited for nothing. That was
# the whole of what this drill reported as engine data loss in three runs of
# six — kill returned, `rm -rf` pulled the data directory out from under a
# server still serving, a new server bound the same port beside it, and the
# writes the kernel routed to the dying one were acknowledged and then went
# with it. The importer recorded their offset as durable, `--resume` skipped
# them, and the digest came up ~12,000 keys short with zero errors anywhere.
stop_server() { # pid port
    kill "$1" 2>/dev/null
    local t0=$SECONDS
    while kill -0 "$1" 2>/dev/null; do
        [ $((SECONDS - t0)) -ge 30 ] && { kill -9 "$1" 2>/dev/null; break; }
        sleep 0.1
    done
    wait_port_free "$2"
}

start_server() { # port dir → pid
    wait_port_free "$1"
    env KEVY_BIND=127.0.0.1 $KEVY --threads 4 --port "$1" --dir "$2" --no-aof >/dev/null 2>&1 &
    local pid=$!
    for _ in $(seq 100); do
        $CLI -p "$1" PING >/dev/null 2>&1 && { echo $pid; return; }
        sleep 0.2
    done
    echo "drill: server on $1 never came up" >&2
    exit 1
}

mkdir -p "$DIR/src" "$DIR/dst"
SPID=$(start_server $SRC "$DIR/src")

echo "== step 1: seed mailrs-shaped keyspace =="
python3 - $SRC <<'PY'
import random, socket, sys, time
port = int(sys.argv[1])
random.seed(42)
s = socket.create_connection(("127.0.0.1", port))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
buf = [b""]
def enc(*p):
    b = b"*%d\r\n" % len(p)
    for x in p:
        x = x.encode() if isinstance(x, str) else x
        b += b"$%d\r\n%s\r\n" % (len(x), x)
    return b
def rd():
    def line():
        while b"\r\n" not in buf[0]:
            _chunk = s.recv(1 << 20)
            if not _chunk:
                raise AssertionError('server closed the connection mid-reply')
            buf[0] += _chunk
        l, _, r = buf[0].partition(b"\r\n"); buf[0] = r; return l
    l = line(); t, body = l[:1], l[1:]
    if t in (b"+", b"-", b":"): return l
    if t == b"$":
        n = int(body)
        if n < 0: return None
        while len(buf[0]) < n + 2:
            _chunk = s.recv(1 << 20)
            if not _chunk:
                raise AssertionError('server closed the connection mid-reply')
            buf[0] += _chunk
        out = buf[0][:n]; buf[0] = buf[0][n+2:]; return out
    if t == b"*": return [rd() for _ in range(int(body))]
WORDS = [f"w{r}" for r in range(30_000)]
def body_text(n):
    return " ".join(WORDS[min(int(30_000 ** random.random()) - 1, 29_999)] for _ in range(n))
t0 = time.time()
batch = []
def flush():
    global batch
    if batch:
        s.sendall(b"".join(batch)); [rd() for _ in batch]; batch = []
N_MSG = 200_000
for i in range(N_MSG):
    words = random.choice((80, 200, 600, 2000))  # ~0.5-12KB bodies
    batch.append(enc("HSET", f"msg:{i}",
                     "frm", f"user{i % 20_000}", "subj", body_text(6),
                     "ts", str(1_700_000_000 + i), "body", body_text(words)))
    batch.append(enc("ZADD", f"mbox:{i % 20_000}", str(1_700_000_000 + i), f"msg:{i}"))
    if i % 40 == 0:
        batch.append(enc("SADD", f"tag:t{i % 1_000}", f"msg:{i}"))
    if len(batch) >= 400:
        flush()
for u in range(20_000):
    batch.append(enc("HSET", f"usr:{u}", "name", f"user{u}", "quota", str(u % 5000)))
    if len(batch) >= 400: flush()
for t in range(50_000):
    batch.append(enc("SET", f"session:{t}", f"tok{t}", "EX", "86400"))
    if len(batch) >= 400: flush()
flush()
print(f"seeded in {time.time()-t0:.1f}s")
PY
$CLI -p $SRC DBSIZE

echo "== step 2: export (timed) =="
T0=$(date +%s)
$CLI export -p $SRC "$DIR/dump.kevy" || { echo "drill: EXPORT FAILED"; exit 1; }
echo "export took $(( $(date +%s) - T0 ))s, size: $(du -h "$DIR/dump.kevy" | cut -f1)"

echo "== step 3: import --strict into fresh server (timed) =="
DPID=$(start_server $DST "$DIR/dst")
T0=$(date +%s)
$CLI import -p $DST --strict "$DIR/dump.kevy" || { echo "drill: IMPORT FAILED"; exit 1; }
echo "import took $(( $(date +%s) - T0 ))s"

echo "== step 4: per-prefix digest/diff both ends =="
DIFF_OK=1
for pfx in msg: mbox: usr: tag: session:; do
    A=$($CLI digest -p $SRC $pfx)
    B=$($CLI digest -p $DST $pfx)
    # session: TTLs decay but digest excludes TTL; counts must match
    if [ "$A" = "$B" ]; then
        echo "  $pfx OK ($A)"
    else
        echo "  $pfx MISMATCH: src=$A dst=$B"
        DIFF_OK=0
    fi
done
[ $DIFF_OK = 1 ] || { echo "drill: DIGEST MISMATCH"; exit 1; }

echo "== step 5: kill -9 mid-import → --resume =="
stop_server $DPID $DST
rm -rf "$DIR/dst"; mkdir -p "$DIR/dst"
DPID=$(start_server $DST "$DIR/dst")
rm -f "$DIR/dump.kevy.progress"
( $CLI import -p $DST --strict "$DIR/dump.kevy" >/dev/null 2>&1 ) &
IMP=$!
sleep 3
kill -9 $IMP 2>/dev/null
MID=$($CLI -p $DST DBSIZE)
echo "  killed importer mid-flight at dbsize=$MID"
T0=$(date +%s)
$CLI import -p $DST --resume --strict "$DIR/dump.kevy" || { echo "drill: RESUME FAILED"; exit 1; }
echo "  resume took $(( $(date +%s) - T0 ))s"
A=$($CLI digest -p $SRC msg:)
B=$($CLI digest -p $DST msg:)
if [ "$A" = "$B" ]; then
    echo "  post-resume msg: digest OK"
else
    # Say what did not match. "MISMATCH" alone cannot distinguish one key
    # from a whole prefix, and the next reader would have to reproduce the
    # kill to learn anything — which is the expensive half.
    echo "drill: POST-RESUME MISMATCH"
    echo "  src digest: $A"
    echo "  dst digest: $B"
    echo "  src msg: keys: $($CLI -p $SRC PREFIX.COUNT msg: 2>&1 | tail -1)"
    echo "  dst msg: keys: $($CLI -p $DST PREFIX.COUNT msg: 2>&1 | tail -1)"
    echo "  src dbsize: $($CLI -p $SRC DBSIZE 2>&1 | tail -1)   dst dbsize: $($CLI -p $DST DBSIZE 2>&1 | tail -1)"
    # Raw key lists, both sides, untouched. The analysis happens outside
    # this script: three attempts to summarise it here got the row format
    # wrong three different ways, and a broken summary of a real defect is
    # worse than the rows.
    $CLI -p $SRC KEYS 'msg:*' > /tmp/drill-src-raw.txt 2>&1
    $CLI -p $DST KEYS 'msg:*' > /tmp/drill-dst-raw.txt 2>&1
    echo "  raw key lists left at /tmp/drill-{src,dst}-raw.txt"
    cp "$DIR/dump.kevy.progress" /tmp/drill-progress.txt 2>/dev/null
    echo "  progress 文件内容: $(cat /tmp/drill-progress.txt 2>/dev/null)"
    exit 1
fi

echo "== step 6: post-load index build (range + TEXT over big bodies) =="
$CLI -p $DST IDX.CREATE m_ts ON PREFIX msg: FIELD ts TYPE i64 KIND range
$CLI -p $DST IDX.CREATE m_body ON PREFIX msg: FIELD body TYPE str KIND text
T0=$(date +%s)
READY=0
for _ in $(seq 600); do
    R=$($CLI -p $DST IDX.QUERY m_body MATCH w0 LIMIT 1 2>&1)
    case "$R" in
        *INDEXBUILDING*|*ERR*) sleep 1 ;;
        *) READY=1; break ;;
    esac
done
[ $READY = 1 ] || { echo "drill: INDEX NEVER READY"; exit 1; }
echo "  text+range backfill ready in $(( $(date +%s) - T0 ))s (200k docs, big bodies)"
$CLI -p $DST IDX.QUERY m_ts RANGE 1700000000 1700000100 LIMIT 5 | head -3

echo "== step 7: copy-prefix --rate / delete-prefix round trip =="
T0=$(date +%s)
$CLI copy-prefix -p $DST usr: usrbak: --rate 5000 || { echo "drill: COPY FAILED"; exit 1; }
echo "  copy-prefix 20k @5000/s took $(( $(date +%s) - T0 ))s (expect ~4s)"
$CLI digest -p $DST usrbak: | grep -q "20000" && echo "  usrbak: count OK"
$CLI delete-prefix -p $DST usrbak: --rate 10000 >/dev/null
$CLI digest -p $DST usrbak: | grep -q "^0 " && echo "  delete-prefix clean"

echo "drill: ALL STEPS PASS"
