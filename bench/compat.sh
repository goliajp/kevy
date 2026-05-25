#!/usr/bin/env bash
# Differential compatibility test: run the SAME command sequence against kevy and
# real valkey 9.1 (both in Docker, driven by the neutral valkey-cli) and diff the
# replies. Both start empty, so an identical sequence must yield identical state.
#
#   `check`  — exact reply match (scalars + order-sensitive: GET/LRANGE/ZRANGE…)
#   `checku` — order-insensitive (unordered collections: HGETALL/SMEMBERS/SINTER…)
set -uo pipefail
cd "$(dirname "$0")"

echo "### bringing up valkey 9.1 + kevy ..."
docker compose up -d --build >/dev/null 2>&1
for h in valkey kevy; do
  for _ in $(seq 1 60); do
    docker compose exec -T loadgen valkey-cli -h "$h" -p 6379 ping 2>/dev/null | grep -q PONG && break
  done
done

run() { docker compose exec -T loadgen valkey-cli -h "$1" -p 6379 "${@:2}" 2>&1; }
strip_idx() { sed -E 's/^[0-9]+\) //'; }

pass=0
fail=0
report() { # label expected_from_valkey actual_from_kevy cmd...
  if [ "$2" = "$3" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "MISMATCH [$1] ${*:4}"
    echo "  valkey: $(printf '%s' "$2" | tr '\n' '|')"
    echo "  kevy  : $(printf '%s' "$3" | tr '\n' '|')"
  fi
}
check() { # cmd... — exact, order-sensitive
  report exact "$(run valkey "$@")" "$(run kevy "$@")" "$@"
}
checku() { # cmd... — order-insensitive (strip "N)" indices, sort)
  local v k
  v=$(run valkey "$@" | strip_idx | sort)
  k=$(run kevy "$@" | strip_idx | sort)
  report unordered "$v" "$k" "$@"
}

echo "### running compatibility checks ..."
# strings
check SET s hello
check GET s
check APPEND s "!"
check STRLEN s
check GETSET s world
check SETNX s nope
check INCR ctr
check INCRBY ctr 41
check DECR ctr
check INCRBYFLOAT f 3.0
check INCRBYFLOAT f 1.5
check MSET a 1 b 2 c 3
check MGET a b missing c
check GETDEL a
check GET a
# generic keys
check EXISTS b c missing
check TYPE s
check EXPIRE s 100
check TTL s
check PERSIST s
check TTL s
check DEL b c
# hash
check HSET h f1 v1 f2 v2
check HSET h f1 v1b
check HGET h f1
check HLEN h
check HEXISTS h f2
check HINCRBY h n 7
checku HKEYS h
checku HGETALL h
check HDEL h f1
# list
check RPUSH l a b c
check LPUSH l z
check LRANGE l 0 -1
check LINDEX l -1
check LLEN l
check LSET l 0 Z
check LRANGE l 0 -1
check LPOP l
check RPOP l
check LREM l 0 b
# set
check SADD st x y z x
check SCARD st
check SISMEMBER st y
checku SMEMBERS st
check SADD st2 y z w
checku SINTER st st2
checku SUNION st st2
checku SDIFF st st2
check SREM st x
# zset
check ZADD z1 2 b 1 a 3 c
check ZADD z1 5 a
check ZSCORE z1 a
check ZCARD z1
check ZRANK z1 c
check ZRANGE z1 0 -1
check ZRANGE z1 0 -1 WITHSCORES
check ZRANGEBYSCORE z1 2 5
check ZCOUNT z1 1 3
check ZINCRBY z1 1 b
check ZREM z1 a
# transactions are stateful per-connection; valkey-cli one-shot can't span them,
# and pub/sub blocks — both are covered by kevy's own integration tests instead.

echo "### compat: $pass passed, $fail mismatched"
docker compose down >/dev/null 2>&1
[ "$fail" -eq 0 ]
