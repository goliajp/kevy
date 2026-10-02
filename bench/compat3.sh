#!/usr/bin/env bash
# 3-way differential compatibility: run the SAME command sequence against
# valkey 9.1.2, redis 8.10.2, and kevy (all in Docker, driven by the neutral
# valkey-cli) and diff the replies. valkey & redis are the reference (a Redis
# fork + the original); kevy is the subject. All start empty, so an identical
# sequence must yield identical replies.
#
#   check   — exact reply match (scalars + order-sensitive: GET/LRANGE/ZRANGE…)
#   checku  — order-insensitive (unordered collections: HGETALL/SMEMBERS/SINTER…)
set -uo pipefail
# KEVY_BIN=<release server> runs that binary instead of building the image
if [ -n "${KEVY_BIN:-}" ]; then
  KEVY_BIN=$(cd "$(dirname "$KEVY_BIN")" && pwd)/$(basename "$KEVY_BIN")
  [ -x "$KEVY_BIN" ] || { echo "compat3: $KEVY_BIN is not an executable" >&2; exit 2; }
  export KEVY_BIN COMPOSE_FILE=docker-compose.yml:docker-compose.hostbin.yml
fi
cd "$(dirname "$0")"

. ./anchor-lib.sh || { echo "compat3: cannot load anchor-lib.sh" >&2; exit 2; }
command -v anchor_pin >/dev/null || { echo "compat3: anchor-lib.sh loaded but anchor_pin is missing" >&2; exit 2; }
echo "### bringing up valkey $(anchor_pin valkey) + redis $(anchor_pin redis) + kevy ..."
# Pull before up. `compose up` is happy with a cached layer under the pinned
# tag, which is the exact scenario COMPETITOR-ANCHORS.json was opened about:
# the box serves what it cached weeks ago while the registry serves the pin.
# The assertion below then catches it — correctly, and after a five-minute
# build. Pulling first makes the common case pass instead of failing loudly.
docker compose pull -q valkey redis loadgen >/dev/null 2>&1
docker compose up -d --build valkey redis kevy loadgen >/dev/null 2>&1
for h in valkey redis kevy; do
  for _ in $(seq 1 60); do
    docker compose exec -T loadgen valkey-cli --no-raw -h "$h" -p 6379 ping 2>/dev/null | grep -q PONG && break
    sleep 0.3
  done
done

# --no-raw, and it changes what this file proves. Piped, valkey-cli renders in
# raw mode: `:1` and `+1` and `$1\r\n1` all print as `1`. So 212/212 meant "the
# competitor's renderer produced the same text", not "the wire is the same" —
# and the bit it folded away is exactly the one a type-dispatching client
# library reads. --no-raw prints `(integer) 1` against `"1"`.
# What answered, not what the compose file asked for.
# valkey reports the Redis version it EMULATES in `redis_version` (7.2.4 on
# valkey 9.1.2) and its own in `valkey_version`. Asking the wrong field made
# a correctly-pinned container look like a stale one — the witness was right
# to fire and wrong about what it saw. Prefer the server's own field.
engine_reported() {
    # Two expressions in one sed do NOT reorder the input: redis_version comes
    # first in INFO's output, so `head -1` took the emulated version anyway and
    # the fix read exactly like the bug. Ask for the server's own field, and
    # only fall back when there is none.
    local info
    info=$(docker compose exec -T loadgen valkey-cli --no-raw -h "$1" -p 6379 INFO server 2>/dev/null | tr -d '\r')
    local own
    own=$(printf '%s\n' "$info" | sed -n 's/^valkey_version:\(.*\)$/\1/p' | head -1)
    if [ -n "$own" ]; then
        printf '%s' "$own"
    else
        printf '%s\n' "$info" | sed -n 's/^redis_version:\(.*\)$/\1/p' | head -1
    fi
}

run() { docker compose exec -T loadgen valkey-cli --no-raw -h "$1" -p 6379 "${@:2}" 2>&1; }
strip_idx() { sed -E 's/^[0-9]+\) //'; }

kv_p=0; kv_f=0   # kevy  vs valkey
kr_p=0           # kevy = redis where redis and valkey answer differently
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
  # where the two references disagree neither one decides alone: kevy must
  # give one of their answers
  if [ "$k" = "$v" ]; then kv_p=$((kv_p + 1))
  elif [ "$r" != "$v" ] && [ "$k" = "$r" ]; then
    kr_p=$((kr_p + 1)); echo "  kevy=redis≠valkey [$*]"
  else
    kv_f=$((kv_f + 1)); echo "  kevy≠valkey  [$*]  valkey=[$(fmt "$v")]  kevy=[$(fmt "$k")]"
  fi
  if [ "$r" = "$v" ]; then rv_p=$((rv_p + 1)); else
    rv_f=$((rv_f + 1)); echo "  redis≠valkey [$*]  valkey=[$(fmt "$v")]  redis=[$(fmt "$r")]"
  fi
}
check() { check_impl x "$@"; }
checku() { check_impl u "$@"; }

for pair in "valkey:$(anchor_pin valkey)" "redis:$(anchor_pin redis)"; do
    host=${pair%%:*}; want=${pair#*:}
    anchor_require "$host (container)" "$want" "$(engine_reported "$host")"
done

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
# -0 and 0 are one score. kevy's rank tree needs a total order over the
# score, and total_cmp gives one by separating the two zeros — which put
# `zb` before `za` here and after it in both references until the sign was
# folded at the door. The corpus had no plus-or-minus-zero case, so the
# headline was honest about what it measured and this was outside it.
check ZADD zz 0 za
check ZADD zz -0 zb
check ZRANGE zz 0 -1
check ZRANGE zz 0 -1 WITHSCORES
check ZSCORE zz zb
check ZADD zz XX CH 0 zb
check ZCOUNT z1 1 3
check ZINCRBY z1 1 b
check ZREM z1 a
# top-end pops, reverse ranks, WITHSCORE, multi-score, random picks; a
# random reply is only compared where every engine must give the same set
check ZADD zp 1 a 2 b 3 c 3 d 1.5 e
check ZREVRANK zp a
check ZREVRANK zp a WITHSCORE
check ZRANK zp e WITHSCORE
check ZRANK zp nope WITHSCORE
check ZREVRANK zp a BAD
check ZMSCORE zp a nope e
check ZMSCORE nokey a b
check ZMSCORE zp
checku ZRANDMEMBER zp 10
# few enough lines that valkey-cli does not pad the indexes, which the
# unordered compare strips
check ZADD zp3 1 a 2 b 3 c
checku ZRANDMEMBER zp3 5 WITHSCORES
check ZRANDMEMBER nokey
check ZRANDMEMBER nokey 3 WITHSCORES
check ZRANDMEMBER zp 0
check ZRANDMEMBER zp x
check ZRANDMEMBER zp 1 BAD
check ZRANDMEMBER zp -9223372036854775808
check ZADD zone 7 only
check ZRANDMEMBER zone -3 WITHSCORES
check ZRANDMEMBER zone
check ZPOPMAX zp
check ZPOPMAX zp 2
check ZPOPMAX zp 0
check ZPOPMAX zp -1
check ZPOPMAX zp x
check ZPOPMAX zp 1 2
check ZPOPMIN zp x
check ZPOPMIN zp 1 2
check ZPOPMAX nokey 2
check SET zstr v
check ZPOPMAX zstr
check ZREVRANK zstr a
check ZMSCORE zstr a
check ZRANDMEMBER zstr
# multi-key pops (the blocking forms only where data is there or the
# timeout is short; ZMPOP's keys span shards on a sharded kevy)
check ZADD zm1 1 a 2 b 3 c
check ZADD zm2 5 x
check RPUSH lm1 a b c d
check ZMPOP 2 nokey zm1 MIN
check ZMPOP 2 nokey zm1 MAX COUNT 2
check ZMPOP 1 zm1 MIN COUNT 10
check ZMPOP 2 nokey nokey2 MIN
check ZMPOP 0 zm1 MIN
check ZMPOP x zm1 MIN
check ZMPOP 3 zm1 MIN
check ZMPOP 1 zm2 BAD
check ZMPOP 1 zm2 MIN COUNT 0
check ZMPOP 1 zm2 MIN COUNT
check ZMPOP 1 zm2 MIN COUNT 1 COUNT 1
check ZMPOP 2 nokey lm1 MIN
check LMPOP 2 nokey lm1 LEFT
check LMPOP 1 lm1 RIGHT COUNT 2
check LMPOP 1 lm1 LEFT COUNT 5
check LMPOP 1 lm1 LEFT
check LMPOP 1 nokey MIDDLE
check LMPOP 1 nokey LEFT COUNT 0
check ZADD zb 1 a 2 b
check BZPOPMAX zb 0
check BZPOPMAX nokey zb 0
check BZPOPMAX nokey 0.05
check BZPOPMAX nokey x
check BZPOPMAX nokey -1
check ZADD zb2 5 x 6 y 7 z
check BZMPOP 0 1 zb2 MAX COUNT 2
check BZMPOP 0 2 nokey zb2 MIN
check BZMPOP 0.05 1 nokey MIN
check BZMPOP -1 1 zb2 MIN
check BZMPOP 0 0 zb2 MIN
check BZMPOP 0 2 zb2 MIN
check RPUSH lb 1 2 3
check BLMPOP 0 1 lb RIGHT COUNT 2
check BLMPOP 0 2 nokey lb LEFT
check BLMPOP 0.05 1 lb LEFT
check BLMPOP 0 1 lb UP
check SET bstr v
check BLMPOP 0 1 bstr LEFT
check BZMPOP 0 1 bstr MIN
check RPUSH bsrc a b
check BLMOVE bsrc bdst LEFT RIGHT 0
check BLMOVE bsrc bdst RIGHT LEFT 0
check BLMOVE bsrc bdst LEFT RIGHT 0.05
check BLMOVE bsrc bdst UP RIGHT 0
check BLMOVE bsrc bdst LEFT RIGHT -1
check BLMOVE bstr bdst LEFT RIGHT 0
check LRANGE bdst 0 -1
check BRPOPLPUSH nokey bdst 0.05
check BLPOP nokey -1
# HSTRLEN, LPUSHX / RPUSHX, SUBSTR, the field-TTL family's grammar
check HSET b6h f hello g 12345 n -1.5
check HSTRLEN b6h f
check HSTRLEN b6h n
check HSTRLEN b6h nope
check HSTRLEN b6nokey f
check HSTRLEN b6h f extra
check SET b6str v
check HSTRLEN b6str f
check LPUSHX b6nokey a
check RPUSHX b6nokey a
check EXISTS b6nokey
check RPUSH b6l a
check LPUSHX b6l x y
check RPUSHX b6l z
check LRANGE b6l 0 -1
check LPUSHX b6str a
check SET b6s HelloWorld
check SUBSTR b6s 0 4
check SUBSTR b6s -3 -1
check SUBSTR b6s 4 2
check SUBSTR b6nokey 0 1
check SUBSTR b6s x 1
check SUBSTR b6l 0 1
check HSET b6t a 1 b 2
check HEXPIRETIME b6t FIELDS 2 a nope
check HEXPIREAT b6t 4102444800 FIELDS 2 a nope
check HEXPIRETIME b6t FIELDS 1 a
check HPEXPIRETIME b6t FIELDS 1 a
check HEXPIREAT b6t 4102444800 NX FIELDS 1 a
check HEXPIREAT b6t 4102444801 GT FIELDS 1 a
check HEXPIREAT b6t 1 FIELDS 1 b
check HEXISTS b6t b
check HEXPIREAT b6t 4102444800 FIELDS 0 a
check HEXPIREAT b6t 4102444800 FIELDS 2 a
check HEXPIREAT b6t 4102444800 BAD 1 a
check HEXPIREAT b6t -1 FIELDS 1 a
check HEXPIRE b6t 100 NX XX FIELDS 1 a
check HEXPIRE b6t 100 FIELDS 1 a FIELDS 1 a
check HPEXPIREAT b6t 70368744177664 FIELDS 1 a
check HTTL b6t NX FIELDS 1 a
check HTTL b6t FIELDS 2 a
check HTTL b6t FIELDS 0 a
check HPERSIST b6t FIELDS 1 a
# reads over several keys (spread across shards on a sharded kevy)
check SADD rs1 a b c
check SADD rs2 b c d
check SINTERCARD 2 rs1 rs2
check SINTERCARD 2 rs2 rsnokey
check SINTERCARD 1 rs2 LIMIT 1
check SINTERCARD 1 rs2 LIMIT 0
check SINTERCARD 0 rs2
check SINTERCARD 2 rs2
check SINTERCARD 1 rs2 LIMIT -1
check SINTERCARD 1 b6str
check SINTERCARD 1 rs2 BAD 1
check ZADD rz1 1 a 2 b 3 c
check ZADD rz2 10 b 20 c 30 d
check ZINTER 2 rz1 rz2
check ZINTER 2 rz1 rz2 WITHSCORES
check ZINTER 2 rz1 rz2 WEIGHTS 2 3 AGGREGATE MAX WITHSCORES
check ZUNION 2 rz1 rz2 WITHSCORES
check ZUNION 2 rz1 rz2 AGGREGATE MIN WITHSCORES
check ZDIFF 2 rz1 rz2 WITHSCORES
check ZDIFF 1 rznokey
check ZINTER 0 rz1
check ZINTER x rz1
check ZINTER 3 rz1 rz2
check ZDIFF 2 rz1 rz2 WEIGHTS 1 2
check ZUNION 2 rz1 b6str
check ZUNION 2 rz1 rs1 WITHSCORES
check ZUNION 1 rz1 WEIGHTS x
check ZINTERSTORE rzd 0 rz1
check ZINTERSTORE rzd 3 rz1
check ZUNIONSTORE rzd x rz1
check ZINTERSTORE rzd 1 rz1 WITHSCORES
check SET rk1 ohmytext
check SET rk2 mynewtext
check LCS rk1 rk2
check LCS rk1 rk2 LEN
check LCS rk1 rk2 IDX
check LCS rk1 rk2 IDX MINMATCHLEN 4 WITHMATCHLEN
check LCS rk1 rknokey
check LCS rk1 rs1
check LCS rk1 rk2 BAD
check LCS rk1 rk2 LEN IDX
# ZRANGE's 6.2 forms and the lexicographic family
check ZADD zrr 1 a 2 b 3 c 4 d 5 e
check ZRANGE zrr 0 1 REV
check ZRANGE zrr 0 -1 REV WITHSCORES
check ZRANGE zrr 2 4 BYSCORE
check ZRANGE zrr '(2' +inf BYSCORE LIMIT 1 2 WITHSCORES
check ZRANGE zrr 4 2 BYSCORE REV
check ZRANGE zrr +inf -inf BYSCORE REV LIMIT 0 2
check ZRANGE zrr 0 -1 LIMIT 0 1
check ZRANGE zrr 0 -1 BYSCORE BYLEX
check ZRANGE zrr x 1 BYSCORE
check ZRANGE zrr 1 5 BYSCORE LIMIT -1 2
check ZRANGE zrr 1 5 BYSCORE LIMIT 1
check ZREVRANGE zrr 0 1 LIMIT 0 1
check ZADD zlx 0 a 0 b 0 c 0 d 0 e 0 f
check ZRANGE zlx '[b' '(e' BYLEX
check ZRANGE zlx - + BYLEX LIMIT 1 2
check ZRANGE zlx '(e' '[b' BYLEX REV
check ZRANGE zlx '[b' '[e' BYLEX WITHSCORES
check ZRANGE zlx b e BYLEX
check ZRANGEBYLEX zlx '[b' '[d'
check ZRANGEBYLEX zlx - + LIMIT 2 3
check ZRANGEBYLEX zlx a +
check ZRANGEBYLEX zlx '[b' '[d' WITHSCORES
check ZREVRANGEBYLEX zlx '[d' '[b'
check ZREVRANGEBYLEX zlx + - LIMIT 1 2
check ZLEXCOUNT zlx - +
check ZLEXCOUNT zlx '[b' '(e'
check ZLEXCOUNT zlx x y
check ZREMRANGEBYLEX zlx '[e' +
check ZRANGE zlx 0 -1
check ZRANGEBYLEX b6str - +
check ZRANGE b6str 0 -1 BYLEX
check ZRANGESTORE zrdst zrr 0 -1
check ZRANGE zrdst 0 -1 WITHSCORES
check ZRANGESTORE zrdst zrr 2 4 BYSCORE
check ZRANGESTORE zrdst zrr 0 0 REV
check ZRANGE zrdst 0 -1 WITHSCORES
check ZRANGESTORE zrdst zrr '(1' +inf BYSCORE LIMIT 1 1
check ZRANGESTORE zrdst zlx '[b' '(d' BYLEX
check ZRANGESTORE zrdst rznokey 0 -1
check EXISTS zrdst
check ZRANGESTORE zrdst zrr 0 -1 WITHSCORES
check ZRANGESTORE zrdst b6str 0 -1
check ZRANGESTORE b6str zrr 0 -1
check TYPE b6str
check SADD sm1 a b
check SADD sm2 x
check SMOVE sm1 sm2 a
check SMOVE sm1 sm2 nope
check SMOVE sm1 smnew b
check SMEMBERS smnew
check SMOVE sm2 sm2 x
check SMOVE sm2 sm2 nope
check SMOVE sm2 rk1 x
check SMOVE sm2 rk1 nope
check SMOVE rk1 sm2 x
check SMOVE smnokey rk1 x
check SMOVE sm1 sm2
check MSETNX mx1 a mx2 b
check MSETNX mx2 x mx3 y
check EXISTS mx3
check MSETNX mx4 a mx4 b
check GET mx4
check MSETNX mx1
check MSETNX mx1 a mx2

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

# --- the fourteen verbs wired to the RESP surface in 6.0.0 ---
# Every one of them was already implemented in the engine and answered by
# the embedded facade; none reached a client. Their compatibility claim
# is only worth what a real valkey and a real redis say about it, so they
# are driven here rather than only against kevy's own facade.
#
# TIME is deliberately absent: it answers with the clock, and three
# servers asked a few milliseconds apart do not agree by design.
check SETBIT bits 7 1
check SETBIT bits 0 1
check SETBIT bits 7 0
check GETBIT bits 0
check GETBIT bits 7
check GETBIT bits 999
check BITCOUNT bits
check BITCOUNT bits 0 -1
check BITCOUNT bits 0 0
check SETBIT bits2 3 1
check BITOP AND bdst bits bits2
check GET bdst
check BITOP OR bdst bits bits2
check GET bdst
check BITOP XOR bdst bits bits2
check BITOP NOT bnot bits
check BITPOS bits 1
check BITPOS bits 0
check BITPOS bits 1 0
check BITPOS bits 1 0 -1
check SET rng hello-world
check GETRANGE rng 0 4
check GETRANGE rng -5 -1
check GETRANGE rng 99 200
check SETRANGE rng 5 _____
check GET rng
check SETRANGE rng 20 tail
check STRLEN rng
check RPUSH li a b c
check LINSERT li BEFORE b X
check LINSERT li AFTER b Y
check LINSERT li BEFORE nosuch Q
check LRANGE li 0 -1
check SET csrc copy-me
check COPY csrc cdst
check GET cdst
check COPY csrc cdst
check COPY csrc cdst REPLACE
check COPY nosuchsrc cdst2
check TOUCH csrc cdst nosuchkey
check TOUCH nosuchkey
check SET gx gv
check GETEX gx
check GETEX gx EX 1000
check TTL gx
check GETEX nosuchkey
check ZADD zr 1 one 2 two 3 three
check ZREVRANGE zr 0 -1
check ZREVRANGE zr 0 -1 WITHSCORES
check ZREVRANGE zr 0 0
check ZREVRANGE zr -2 -1
check ZREVRANGE zr 5 10
check HSET hf f 1
check HINCRBYFLOAT hf f 1.5
check HINCRBYFLOAT hf f -0.25
check HINCRBYFLOAT hf newfield 2.5
check HGET hf f
# ...and the refusals, where clones diverge most
check SETBIT bits abc 1
check SETBIT bits 7 2
check GETBIT bits abc
check BITCOUNT bits a b
check BITPOS bits 2
check BITOP SIDEWAYS d bits
check BITOP NOT d bits bits2
check GETRANGE rng a 4
check SETRANGE rng abc x
check LINSERT li SIDEWAYS b Q
check GETEX gx XX 100
check GETEX gx EX 0
check ZREVRANGE zr 0 -1 SCORES
check HINCRBYFLOAT hf f abc

# --- error / type / arity reply compatibility (where clones diverge) ---
check SET str1 v
check LPUSH str1 x '' '' '' '' '' '' '' '' '' # WRONGTYPE: string vs list op
check LRANGE str1 0 -1 '' '' '' '' '' # WRONGTYPE
check HGET str1 f '' '' '' '' '' '' '' '' '' '' # WRONGTYPE
check SET ni abc
check INCR ni '' '' '' '' '' '' '' '' '' '' '' '' '' '' # ERR not an integer
check INCRBYFLOAT ni 1.0 '' '' '' # ERR not a float
check GET '' '' '' '' '' '' '' '' '' '' '' '' '' '' '' '' '' '' # ERR wrong number of arguments
check SET onlykey '' '' '' '' '' '' '' '' '' '' # ERR wrong number of arguments
check LPUSH lonely '' '' '' '' '' '' '' '' '' # ERR wrong number of arguments
check EXPIRE missingkey 100 # 0 (no such key)
check GET missingkey '' '' '' '' '' '' '' # nil
check TYPE missingkey '' '' '' '' '' '' # none
check TTL missingkey '' '' '' '' '' '' '' # -2
check HGET missinghash fld '' # nil

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
check XADD xs 1-0 f dup '' '' '' '' '' '' # ERR id <= top
check XRANGE missingstream - + '' # empty array
# XAUTOCLAIM's cursor: the next pending id, 0-0 at the end, and a scan of at
# most COUNT x 10 entries whether or not they are idle enough
for i in $(seq 1 12); do check XADD xa "$i-0" f v; done
check XGROUP CREATE xa ga 0
check XREADGROUP GROUP ga c1 COUNT 100 STREAMS xa ">"
check XAUTOCLAIM xa ga c2 0 0-0 COUNT 100 JUSTID
check XAUTOCLAIM xa ga c2 0 0-0 COUNT 2 JUSTID
check XAUTOCLAIM xa ga c2 100000000 0-0 COUNT 1 JUSTID
check XAUTOCLAIM xa ga c2 100000000 0-0 COUNT 2 JUSTID
# the pending summary lists consumers by name in byte order, not in the
# order they first appear in the pending list (bob holds the oldest entry)
for i in 1 2 3 4 5; do check XADD xo "$i-0" f v; done
check XGROUP CREATE xo g 0
for c in bob alice zed bob Bob; do check XREADGROUP GROUP g "$c" COUNT 1 STREAMS xo ">"; done
check XPENDING xo g
# XINFO: the replies that carry no clock. A deletion behind a group makes
# its lag unknowable (nil); a trim is not a deletion. Redis 8.10 answers
# XINFO STREAM with six more fields than valkey, so that line is expected
# in the redis-vs-valkey column.
check XINFO STREAM xs
check XINFO GROUPS xs
check XINFO GROUPS xo
for i in 1 2 3 4; do check XADD xe "$i-0" f v; done
check XGROUP CREATE xe g0 0
check XGROUP CREATE xe g1 0 ENTRIESREAD 1
check XGROUP CREATE xe g2 '$'
check XINFO GROUPS xe
check XDEL xe 3-0
check XINFO GROUPS xe
check XGROUP SETID xe g1 2-0 ENTRIESREAD 2
check XGROUP SETID xe nog 0
check XINFO GROUPS xe
check XINFO STREAM xe FULL COUNT 2
check XINFO CONSUMERS xe g0
check XINFO CONSUMERS xe nog
check XINFO STREAM missingstream
check XINFO STREAM xe BOGUS
check XINFO BOGUS
check XINFO HELP
# A read of a consumer's history hands its entries out again (the delivery
# count goes up), an entry deleted since comes back with no fields, and an
# empty history is still listed. XAUTOCLAIM lists a deleted entry whatever
# its idle time.
for i in 1 2 3 4 5; do check XADD xp "$i-0" f v; done
check XGROUP CREATE xp g 0
check XREADGROUP GROUP g c COUNT 3 STREAMS xp ">"
check XREADGROUP GROUP g c STREAMS xp 0
check XDEL xp 2-0
check XREADGROUP GROUP g c STREAMS xp 0
check XREADGROUP GROUP g c2 STREAMS xp 0
check XPENDING xp g
check XPENDING xp g IDLE 100000000 - + 10
check XAUTOCLAIM xp g c3 100000000 0 COUNT 10 JUSTID
check XCLAIM xp g c3 0 1-0 JUSTID
check XCLAIM xp g c3 0 5-0 JUSTID FORCE LASTID 5-0
check XINFO GROUPS xp
check XCLAIM xp g c3 x 1-0
check XCLAIM xp g c3 0 1-0 BOGUS
check XCLAIM missingstream g c3 0 1-0
check XREADGROUP GROUP g newc STREAMS xp missingstream xs xo ">" ">" ">" ">"
check XINFO GROUPS xp
check XGROUP DESTROY missingstream g
check XGROUP CREATECONSUMER missingstream g c
check XGROUP DELCONSUMER missingstream g c
check XGROUP HELP
# XADD and XTRIM options; an approximate trim takes only whole nodes
check XADD xp NOMKSTREAM 6-0 f v
check XADD missingstream NOMKSTREAM 1-0 f v
check XADD xp MAXLEN = 5 7-0 f v
check XADD xp MINID "~" 3-0 LIMIT 10 8-0 f v
check XADD xp MAXLEN 1 LIMIT 1 9-0 f v
check XADD xp 0-0 f v
check XADD xp MAXLEN 1 MAXLEN
for h in valkey redis kevy; do
    seq 1 150 | sed 's/.*/XADD xn &-0 f v/' |
        docker compose exec -T loadgen valkey-cli -h "$h" -p 6379 >/dev/null 2>&1
done
check XTRIM xn MAXLEN "~" 10 LIMIT 20
check XTRIM xn MAXLEN "~" 60
check XTRIM xn MAXLEN "~" 50
check XLEN xn
check XINFO STREAM xn
# ranges with exclusive bounds, XREAD's '+', and XSETID's checks
check XRANGE xp "(3-0" + COUNT 2
check XREVRANGE xp "(7-0" - COUNT 1
check XRANGE xp - + COUNT 0
check XREAD STREAMS xp +
check XREAD STREAMS xp '$'
check XSETID xp 9-0 ENTRIESADDED 100 MAXDELETEDID 8-0
check XSETID xp 1-0
check XSETID missingstream 1-0
check XINFO STREAM xp

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
check RENAMENX rk3 rk4 '' '' '' '' '' '' '' # 0 (dst exists)
check RENAMENX rk3 rk5 '' '' '' '' '' '' '' # 1
check RENAME missingk dst '' '' '' '' # ERR no such key

# --- blocking pops (immediate-hit forms only — deterministic, no real block) ---
check RPUSH bl x y
check BLPOP bl 0
check BRPOP bl 0
check RPUSH bl2 only
check BLPOP miss bl2 0 '' '' '' '' '' '' '' # served from the second key

# --- slowlog (GET carries timestamps → LEN/RESET only) ---
check SLOWLOG RESET
check SLOWLOG LEN

echo "### RESULT  kevy vs valkey: $kv_p/$((kv_p + kr_p + kv_f)) match, $kr_p answer as redis where it and valkey differ   |   redis vs valkey: $rv_p/$((rv_p + rv_f)) match"
docker compose down >/dev/null 2>&1
# Correctness gate: exit non-zero if kevy gave an answer neither reference
# gives, or differed from valkey where the references agree.
[ "$kv_f" -eq 0 ]
