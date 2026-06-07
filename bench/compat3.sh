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

# --- expanded coverage (2026-05-26): gap commands ---
# string / expiry variants (TTL checked immediately so it's still deterministic;
# PTTL/exact-ms skipped — timing-dependent across servers)
check DECRBY ctr 5
check SETEX se2 100 v2
check TTL se2
check GET se2
check PSETEX pse 100000 v3
check GET pse
check SET pe pv
check PEXPIRE pe 100000
check GET pe
# hash gaps
check HSET hh a 1 b 2
check HMGET hh a missing b
check HSETNX hh a 9
check HSETNX hh c 3
check HGET hh c
checku HVALS hh
# list gap
check RPUSH lt a b c d e
check LTRIM lt 1 3
check LRANGE lt 0 -1

# --- error / type / arity reply compatibility (where clones diverge) ---
check SET str1 v
check LPUSH str1 x          # WRONGTYPE: string vs list op
check LRANGE str1 0 -1      # WRONGTYPE
check HGET str1 f           # WRONGTYPE
check SET ni abc
check INCR ni               # ERR not an integer
check INCRBYFLOAT ni 1.0    # ERR not a float
check GET                   # ERR wrong number of arguments
check SET onlykey           # ERR wrong number of arguments
check LPUSH lonely          # ERR wrong number of arguments
check EXPIRE missingkey 100 # 0 (no such key)
check GET missingkey        # nil
check TYPE missingkey       # none
check TTL missingkey        # -2
check HGET missinghash fld  # nil

# --- streams (explicit IDs — `*` auto-ID is wall-clock, non-deterministic) ---
check XADD xs 1-0 f a
check XADD xs 2-0 f b
check XADD xs 3-0 f c
check XLEN xs
check XRANGE xs - +
check XREVRANGE xs + -
check XRANGE xs 2-0 3-0
check XREAD COUNT 10 STREAMS xs 0
check XDEL xs 2-0
check XLEN xs
check XGROUP CREATE xs g1 0
check XREADGROUP GROUP g1 c1 COUNT 10 STREAMS xs ">"
check XACK xs g1 1-0
check XADD xs 4-0 f d
check XTRIM xs MAXLEN 2
check XLEN xs
check XADD xs 1-0 f dup       # ERR id <= top
check XRANGE missingstream - +  # empty array

# --- geo (precision-sensitive: byte-exact match IS the test; if redis≠valkey
#     too on a line, it's float formatting in the references, not a kevy gap) ---
check GEOADD geo 13.361389 38.115556 Palermo
check GEOADD geo 15.087269 37.502669 Catania
check GEODIST geo Palermo Catania
check GEODIST geo Palermo Catania km
check GEOHASH geo Palermo Catania
check GEOPOS geo Palermo Catania
check GEOSEARCH geo FROMMEMBER Palermo BYRADIUS 300 km ASC

# --- rename ---
check SET rk1 rv
check RENAME rk1 rk2
check GET rk2
check EXISTS rk1
check SET rk3 a
check SET rk4 b
check RENAMENX rk3 rk4        # 0 (dst exists)
check RENAMENX rk3 rk5        # 1
check RENAME missingk dst     # ERR no such key

# --- blocking pops (immediate-hit forms only — deterministic, no real block) ---
check RPUSH bl x y
check BLPOP bl 0
check BRPOP bl 0
check RPUSH bl2 only
check BLPOP miss bl2 0        # served from the second key

# --- slowlog (GET carries timestamps → LEN/RESET only) ---
check SLOWLOG RESET
check SLOWLOG LEN

echo "### RESULT  kevy vs valkey: $kv_p/$((kv_p + kv_f)) match   |   redis vs valkey: $rv_p/$((rv_p + rv_f)) match"
docker compose down >/dev/null 2>&1
# Correctness gate: exit non-zero if kevy diverged from valkey on any check
# (redis-vs-valkey diffs are informational — reference float formatting).
[ "$kv_f" -eq 0 ]
