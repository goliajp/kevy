# shellcheck shell=bash
# Sourced by perfgate2.sh: the server, the warm-up and the load generators of
# every angle, on the 4-core topology.
#
# Four shards on 0-3, one per physical core; their SMT siblings 8-11 stay
# empty; the load gets both threads of cores 4-7. Generators run until they
# are killed: N only has to outlast the observation.

# hashtags pinning shard 0..3 under the contiguous slot split (CRC16 slot * 4 >> 14)
TAGS4=(t3 t2 t1 t0)
N_GEN=${N_GEN:-2000000000}
N_ZALG=${N_ZALG:-2000000}
N_HYBRID=${N_HYBRID:-20000}
PORT=7001

server_stop() {
  # only ever the pid this gate spawned: no pattern kill
  [ -n "${SRV:-}" ] || return 0
  kill "$SRV" 2>/dev/null
  while kill -0 "$SRV" 2>/dev/null; do sleep 0.05; done
  SRV=""
}

# $1 = binary, $2 = cpus, $3 = extra flags; KEVY_MUTANT_SPIN passes through
# when set (only a mutant build reads it)
server_start() {
  server_stop
  rm -rf "$RUNDIR/data" && mkdir -p "$RUNDIR/data"
  # shellcheck disable=SC2086 # $3 is zero or more flags
  taskset -c "$2" env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 \
    ${KEVY_MUTANT_SPIN:+KEVY_MUTANT_SPIN=$KEVY_MUTANT_SPIN} \
    "$1" --threads "$SRV_THREADS" --port "$PORT" $3 --no-aof --dir "$RUNDIR/data" \
    >"$RUNDIR/srv.log" 2>&1 &
  SRV=$!
  for _ in $(seq 1 100); do
    [ "$(timeout 2 redis-cli -p "$PORT" PING 2>/dev/null)" = PONG ] && return 0
    sleep 0.1
  done
  refuse "server did not come up (see $RUNDIR/srv.log)"
}

warm_cluster() {
  local i pids=()
  for i in 0 1 2 3; do
    redis-benchmark -p $((PORT + 1 + i)) -n 1000000 -r 1000000 -P 64 -q \
      SET "{${TAGS4[$i]}}:__rand_int__" v >/dev/null 2>&1 &
    pids+=($!)
  done
  wait "${pids[@]}"
}

warm_legacy() {
  redis-benchmark -p "$PORT" -t set -n 300000 -P 64 -q >/dev/null 2>&1
}

warm_zalg() {
  local i j args
  for i in $(seq 0 9); do
    args=""
    for j in $(seq 0 99); do args="$args $((i*100+j)) m$((i*100+j))"; done
    # shellcheck disable=SC2086 # score/member pairs, split on purpose
    redis-cli -p "$PORT" ZADD zalg:a $args >/dev/null 2>&1
    # shellcheck disable=SC2086
    redis-cli -p "$PORT" ZADD zalg:b $args >/dev/null 2>&1
  done
}

# the command one per-shard generator sends, with {TAG} standing for its tag
pinned_cmd() {
  case $1 in
    pinned_cluster_get|pinned_compat_get) echo 'GET {TAG}:__rand_int__' ;;
    pinned_cluster_set|pinned_compat_set) echo 'SET {TAG}:__rand_int__ v' ;;
    pinned_incr)  echo 'INCR {TAG}:c' ;;
    pinned_sadd)  echo 'SADD {TAG}:s __rand_int__' ;;
    pinned_hset)  echo 'HSET {TAG}:h __rand_int__ v' ;;
    pinned_lpush) echo 'LPUSH {TAG}:l v' ;;
    pinned_zadd)  echo 'ZADD {TAG}:z __rand_int__ m__rand_int__' ;;
  esac
}

# One generator per shard, two threads each: 8 threads on the 8 load CPUs.
# Cluster angles connect to the shard's own port, so nothing is forwarded;
# compat angles connect to the shared port and land wherever REUSEPORT puts them.
start_pinned() { # $1 = angle, $2 = cluster|compat
  local i port tag cmd
  GENS=""
  for i in 0 1 2 3; do
    port=$PORT; [ "$2" = cluster ] && port=$((PORT + 1 + i))
    tag=${TAGS4[$i]}
    cmd=$(pinned_cmd "$1")
    # shellcheck disable=SC2086 # the command is split into its argv on purpose
    redis-benchmark -p "$port" -n "$N_GEN" -r 1000000 -c 12 -P 256 --threads 2 -q \
      ${cmd//TAG/$tag} >/dev/null 2>&1 &
    GENS="$GENS${GENS:+,}$!:2"
  done
}

# Keys with no tag, uniform over the slots, on the shared port: whichever
# shard a connection lands on, 3 of 4 commands belong to another shard, so
# the forwarded share is fixed by construction instead of by connection luck.
start_xshard() {
  local i
  GENS=""
  for i in 0 1 2 3; do
    redis-benchmark -p "$PORT" -n "$N_GEN" -r 1000000 -c 12 -P 256 --threads 2 -q \
      SET key:__rand_int__ v >/dev/null 2>&1 &
    GENS="$GENS${GENS:+,}$!:2"
  done
}

start_legacy() { # $1 = redis-benchmark test name
  redis-benchmark -h 127.0.0.1 -p "$PORT" -t "$1" -n "$N_GEN" -c 50 -P 256 \
    --threads "$CLI_THREADS" -q >/dev/null 2>&1 &
  GENS="$!:$CLI_THREADS"
}

start_zalg() {
  redis-benchmark -h 127.0.0.1 -p "$PORT" -n "$N_ZALG" -c 50 -P 16 \
    --threads "$CLI_THREADS" -q ZINTERSTORE "zalg:dst:__rand_int__" 2 zalg:a zalg:b \
    >/dev/null 2>&1 &
  GENS="$!:$CLI_THREADS"
}

# Boot the angle's server on $2 cpus from binary $1, warm it, start its load.
# Sets SRV and GENS (empty for the latency angle).
angle_up() { # $1 = binary, $2 = cpus, $3 = angle
  GENS=""
  case $3 in
    pinned_cluster_*)
      server_start "$1" "$2" --cluster; warm_cluster; start_pinned "$3" cluster ;;
    pinned_compat_*)
      server_start "$1" "$2" --cluster; warm_cluster; start_pinned "$3" compat ;;
    pinned_*)
      server_start "$1" "$2" --cluster; start_pinned "$3" cluster ;;
    xshard_forward_set)
      server_start "$1" "$2" --cluster; warm_cluster; start_xshard ;;
    legacy_4sh_*)
      server_start "$1" "$2" ""; warm_legacy; start_legacy "${3#legacy_4sh_}" ;;
    zalg_zinterstore)
      server_start "$1" "$2" ""; warm_legacy; warm_zalg; start_zalg ;;
    hybrid_p95_us)
      server_start "$1" "$2" ""; python3 "$HERE/perfgate_hybrid.py" load "$PORT" >&2 ;;
    *) refuse "unknown angle $3" ;;
  esac
}

angle_down() {
  local g pids=()
  for g in ${GENS//,/ }; do pids+=("${g%%:*}"); done
  if [ ${#pids[@]} -gt 0 ]; then
    kill "${pids[@]}" 2>/dev/null
    wait "${pids[@]}" 2>/dev/null
  fi
  GENS=""
  server_stop
}
