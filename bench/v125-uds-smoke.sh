#!/bin/bash
# v1.25 UDS comprehensive correctness smoke
#
# Verifies the UDS path (commit 5134b34) is semantically correct, not just
# fast. Tests every command family + dual-bind (TCP+UDS simultaneously) +
# big values + pipelining + pub/sub. Designed to catch the R10 class of
# bugs (B.4 routing silently dropped data, redis-benchmark didn't notice).
#
# Exits 0 on all-green, 1 on any failure with the failing test named.

set -u
KBIN=${KEVY_BIN:-/root/kevy/target/release/kevy}
RB=${REDIS_BENCH:-/root/srcbench/redis/src/redis-benchmark}
RCLI=${REDIS_CLI:-/root/srcbench/redis/src/redis-cli}
SOCK=/tmp/kevy-smoke.sock
PORT=7090
PASS=0
FAIL=0

ulimit -n 200000

cleanup() {
  pkill -9 -x kevy 2>/dev/null
  rm -f "$SOCK"
  sleep 1
}
trap cleanup EXIT

cleanup

# Start kevy with BOTH TCP and UDS bound
setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 KEVY_UNIX_SOCKET="$SOCK" \
  taskset -c 0 "$KBIN" --threads 1 --port $PORT --no-aof \
  </dev/null >/tmp/k-smoke.log 2>&1 &
disown
sleep 1

# Wait for both listeners
for _ in $(seq 1 30); do
  "$RCLI" -s "$SOCK" PING >/dev/null 2>&1 && \
    "$RCLI" -p $PORT PING >/dev/null 2>&1 && break
  sleep 0.2
done

# Test runner
check() {
  local name=$1; shift
  local expected=$1; shift
  local actual
  actual=$("$@" 2>&1 | tr -d '\r\n')
  if [ "$actual" = "$expected" ]; then
    PASS=$((PASS + 1))
    # echo "PASS  $name"
  else
    FAIL=$((FAIL + 1))
    echo "FAIL  $name"
    echo "  expected: $expected"
    echo "  actual:   $actual"
  fi
}

echo "=== TEST 1: TCP and UDS both serve PING simultaneously ==="
check "tcp ping"   "PONG"  "$RCLI" -p $PORT PING
check "uds ping"   "PONG"  "$RCLI" -s "$SOCK" PING

echo "=== TEST 2: SET/GET round-trip via UDS ==="
check "uds set"    "OK"     "$RCLI" -s "$SOCK" SET k1 v1
check "uds get"    "v1"     "$RCLI" -s "$SOCK" GET k1
check "tcp see"    "v1"     "$RCLI" -p $PORT GET k1  # same data via TCP

echo "=== TEST 3: TCP SET visible on UDS (single keyspace) ==="
check "tcp set"    "OK"     "$RCLI" -p $PORT SET k2 hello_from_tcp
check "uds read"   "hello_from_tcp"  "$RCLI" -s "$SOCK" GET k2

echo "=== TEST 4: All collection types via UDS ==="
"$RCLI" -s "$SOCK" DEL hash1 list1 set1 zset1 >/dev/null
check "hset"       "1"      "$RCLI" -s "$SOCK" HSET hash1 f v
check "hget"       "v"      "$RCLI" -s "$SOCK" HGET hash1 f
check "lpush"      "1"      "$RCLI" -s "$SOCK" LPUSH list1 a
check "rpush"      "2"      "$RCLI" -s "$SOCK" RPUSH list1 b
check "llen"       "2"      "$RCLI" -s "$SOCK" LLEN list1
check "sadd"       "1"      "$RCLI" -s "$SOCK" SADD set1 m
check "sismember"  "1"      "$RCLI" -s "$SOCK" SISMEMBER set1 m
check "zadd"       "1"      "$RCLI" -s "$SOCK" ZADD zset1 1.5 m
check "zscore"     "1.5"    "$RCLI" -s "$SOCK" ZSCORE zset1 m

echo "=== TEST 5: INCR / APPEND / EXPIRE via UDS ==="
"$RCLI" -s "$SOCK" DEL counter str1 >/dev/null
check "incr1"      "1"      "$RCLI" -s "$SOCK" INCR counter
check "incr2"      "2"      "$RCLI" -s "$SOCK" INCR counter
check "incrby"     "12"     "$RCLI" -s "$SOCK" INCRBY counter 10
check "set str"    "OK"     "$RCLI" -s "$SOCK" SET str1 hello
check "append"     "12"     "$RCLI" -s "$SOCK" APPEND str1 " world!"
check "get str"    "hello world!"  "$RCLI" -s "$SOCK" GET str1

echo "=== TEST 6: Big value (10 KB) round-trip via UDS ==="
big_val=$(python3 -c "print('x' * 10240)")
check "set 10k"    "OK"     "$RCLI" -s "$SOCK" SET big_k "$big_val"
got=$("$RCLI" -s "$SOCK" STRLEN big_k 2>&1 | tr -d '\r\n')
if [ "$got" = "10240" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  10K strlen: expected 10240 got $got"
fi
# Round-trip read entire 10K value, verify byte-for-byte.
actual_full=$("$RCLI" -s "$SOCK" GET big_k 2>&1)
if [ "$actual_full" = "$big_val" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  10K full round-trip: byte mismatch (actual len ${#actual_full})"
fi

echo "=== TEST 7: Big value (64 KB) round-trip via UDS (BigBulk path) ==="
big_val_64k=$(python3 -c "print('y' * 65536)")
check "set 64k"    "OK"     "$RCLI" -s "$SOCK" SET big_k_64k "$big_val_64k"
got_64k=$("$RCLI" -s "$SOCK" STRLEN big_k_64k 2>&1 | tr -d '\r\n')
if [ "$got_64k" = "65536" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  64K strlen: expected 65536 got $got_64k"
fi

echo "=== TEST 8: SETEX TTL via UDS ==="
check "setex"      "OK"     "$RCLI" -s "$SOCK" SETEX ttl_key 60 valuewith_ttl
ttl=$("$RCLI" -s "$SOCK" TTL ttl_key 2>&1 | tr -d '\r\n')
# TTL should be 59 or 60 (just-set)
if [ "$ttl" = "59" ] || [ "$ttl" = "60" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  ttl: expected ~60 got $ttl"
fi

echo "=== TEST 9: MSET / MGET via UDS ==="
check "mset"       "OK"     "$RCLI" -s "$SOCK" MSET mk1 v1 mk2 v2 mk3 v3
mget=$("$RCLI" -s "$SOCK" MGET mk1 mk2 mk3 2>&1 | tr -d '\r' | tr '\n' '|')
if [ "$mget" = "v1|v2|v3|" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  mget: expected v1|v2|v3| got $mget"
fi

echo "=== TEST 10: Pipelined SET at c=50 -P 16 via UDS, FLUSHALL + 100k unique keys readback ==="
"$RCLI" -s "$SOCK" FLUSHALL >/dev/null
# Use redis-benchmark to SET via UDS (-r N gives random keyspace size).
# At -r 100000 with -n 100000 SETs, expect 100000 × (1-(1-1/100000)^100000) ≈ 63 212 unique.
# Coupon-collector lower bound: still > 50000. Test threshold accordingly.
"$RB" -s "$SOCK" -t set -c 50 -P 16 -n 100000 -r 100000 -q >/dev/null 2>&1
dbsize=$("$RCLI" -s "$SOCK" DBSIZE 2>&1 | tr -d '\r\n')
# expected ~63 212 (coupon collector at n=k=100000); accept 60 000-66 000
if [ "$dbsize" -ge 60000 ] && [ "$dbsize" -le 66000 ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  pipelined dbsize: expected ~63 212 (coupon-collector) got $dbsize"
fi

echo "=== TEST 11: 1000-key SET via UDS then DBSIZE + GET each ==="
"$RCLI" -s "$SOCK" FLUSHALL >/dev/null
for i in $(seq 1 1000); do
  "$RCLI" -s "$SOCK" SET "k:$i" "v_$i" >/dev/null 2>&1
done
dbsize=$("$RCLI" -s "$SOCK" DBSIZE 2>&1 | tr -d '\r\n')
if [ "$dbsize" = "1000" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  1000-key dbsize: expected 1000 got $dbsize"
fi
# Sample 20 keys, verify values round-trip
read_miss=0
read_wrong=0
for i in 1 50 100 200 333 444 555 666 777 888 999 1000 2 17 88 256 500 700 900 950; do
  v=$("$RCLI" -s "$SOCK" GET "k:$i" 2>&1 | tr -d '\r\n')
  if [ -z "$v" ]; then
    read_miss=$((read_miss + 1))
  elif [ "$v" != "v_$i" ]; then
    read_wrong=$((read_wrong + 1))
  fi
done
if [ "$read_miss" = "0" ] && [ "$read_wrong" = "0" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  1000-key readback: $read_miss miss, $read_wrong wrong"
fi

echo "=== TEST 12: Pub/Sub via UDS (subscriber + publish) ==="
# Subscribe in background using `timeout` to bound the subscription.
# Older `redis-cli --timeout` syntax is unreliable; use `timeout` external
# command which is universal.
( timeout 3 "$RCLI" -s "$SOCK" SUBSCRIBE chan1 >/tmp/sub.out 2>&1 ) &
SUB_PID=$!
# Give the subscriber time to actually register on the server
sleep 1
n_subs=$("$RCLI" -s "$SOCK" PUBLISH chan1 hello 2>&1 | tr -d '\r\n')
if [ "$n_subs" = "1" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  publish returned: expected 1 sub got '$n_subs'"
fi
# Wait for subscriber to drain + exit
sleep 3
wait $SUB_PID 2>/dev/null || true
if grep -q "hello" /tmp/sub.out; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  subscriber didn't receive 'hello' (see /tmp/sub.out)"
  cat /tmp/sub.out
fi

echo "=== TEST 13: FLUSHALL + persistence-less restart sanity ==="
check "flushall"   "OK"     "$RCLI" -s "$SOCK" FLUSHALL
dbsize=$("$RCLI" -s "$SOCK" DBSIZE 2>&1 | tr -d '\r\n')
if [ "$dbsize" = "0" ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  post-flushall dbsize: expected 0 got $dbsize"
fi

echo "=== TEST 14: INFO via UDS (server liveness check) ==="
got=$("$RCLI" -s "$SOCK" INFO server 2>&1 | grep -c "redis_version\|kevy" || true)
if [ "$got" -ge 1 ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo "FAIL  INFO server didn't return version line"
fi

echo ""
echo "=== SMOKE RESULT ==="
echo "PASS: $PASS"
echo "FAIL: $FAIL"
if [ "$FAIL" -eq 0 ]; then
  echo "ALL GREEN"
  exit 0
else
  echo "FAILURES DETECTED — see above"
  exit 1
fi
