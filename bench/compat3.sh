#!/usr/bin/env bash
# 3-way differential compatibility: run the SAME command sequence against
# valkey 9.1, redis 7.4, and kevy (all in Docker, driven by the neutral
# valkey-cli) and diff the replies. valkey & redis are the reference (a Redis
# fork + the original); kevy is the subject. All start empty, so an identical
# sequence must yield identical replies.
#
#   check   — exact reply match (scalars + order-sensitive: GET/LRANGE/ZRANGE…)
#   checku  — order-insensitive (unordered collections: HGETALL/SMEMBERS/SINTER…)
set -uo pipefail
cd "$(dirname "$0")"

echo "### bringing up valkey 9.1 + redis 7.4 + kevy ..."
docker compose up -d --build valkey redis kevy loadgen >/dev/null 2>&1
for h in valkey redis kevy; do
  for _ in $(seq 1 60); do
    docker compose exec -T loadgen valkey-cli -h "$h" -p 6379 ping 2>/dev/null | grep -q PONG && break
    sleep 0.3
  done
done

run() { docker compose exec -T loadgen valkey-cli -h "$1" -p 6379 "${@:2}" 2>&1; }
strip_idx() { sed -E 's/^[0-9]+\) //'; }

kv_p=0; kv_f=0   # kevy  vs valkey
rv_p=0; rv_f=0   # redis vs valkey
fmt() { printf '%s' "$1" | tr '\n' '|'; }
check_impl() {
  local mode=$1; shift
  local v r k
  if [ "$mode" = u ]; then
    v=$(run valkey "$@" | strip_idx | sort)
    r=$(run redis "$@" | strip_idx | sort)
    k=$(run kevy "$@" | strip_idx | sort)
  else
    v=$(run valkey "$@"); r=$(run redis "$@"); k=$(run kevy "$@")
  fi
  if [ "$k" = "$v" ]; then kv_p=$((kv_p + 1)); else
    kv_f=$((kv_f + 1)); echo "  kevy≠valkey  [$*]  valkey=[$(fmt "$v")]  kevy=[$(fmt "$k")]"
  fi
  if [ "$r" = "$v" ]; then rv_p=$((rv_p + 1)); else
    rv_f=$((rv_f + 1)); echo "  redis≠valkey [$*]  valkey=[$(fmt "$v")]  redis=[$(fmt "$r")]"
  fi
}
check() { check_impl x "$@"; }
checku() { check_impl u "$@"; }

echo "### running 3-way compatibility checks ..."
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

echo "### RESULT  kevy vs valkey: $kv_p/$((kv_p + kv_f)) match   |   redis vs valkey: $rv_p/$((rv_p + rv_f)) match"
docker compose down >/dev/null 2>&1
