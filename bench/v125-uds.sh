#!/bin/bash
# A.X UDS bench — kevy vs valkey vs redis, all over Unix-domain socket.
# Fair comparison: removes TCP loopback floor from all three; remaining gap is
# server-side per-op CPU + scheduling.
set -u
KBIN=/root/kevy/target/release/kevy
VBIN=/root/srcbench/valkey/src/valkey-server
RBIN=/root/srcbench/redis/src/redis-server
RB=/root/srcbench/redis/src/redis-benchmark
SOCK=/tmp/kevy.sock
VSOCK=/tmp/valkey.sock
RSOCK=/tmp/redis.sock
ulimit -n 200000

kill_all() {
  pkill -9 -x kevy 2>/dev/null
  pkill -9 -x valkey-server 2>/dev/null
  pkill -9 -x redis-server 2>/dev/null
  rm -f $SOCK $VSOCK $RSOCK
  sleep 1.5
}

wait_ready() {
  local sock=$1
  for _ in $(seq 1 30); do
    "$RB" -s "$sock" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy() {
  kill_all
  # kevy still needs a TCP port even when using UDS (it dual-listens). Use port 0 isn't supported; use a non-conflicting one.
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 KEVY_UNIX_SOCKET=$SOCK taskset -c 0 "$KBIN" \
    --threads 1 --port 7099 --no-aof </dev/null >/tmp/k.log 2>&1 &
  disown
  sleep 1.5
  wait_ready "$SOCK"
}

start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port 0 --unixsocket "$VSOCK" --unixsocketperm 777 \
    --bind 127.0.0.1 --protected-mode no --save '' --appendonly no \
    --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/v.log 2>&1 &
  disown
  sleep 1.5
  wait_ready "$VSOCK"
}

start_redis() {
  kill_all
  setsid taskset -c 0-9 "$RBIN" --port 0 --unixsocket "$RSOCK" --unixsocketperm 777 \
    --bind 127.0.0.1 --protected-mode no --save '' --appendonly no \
    --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/r.log 2>&1 &
  disown
  sleep 1.5
  wait_ready "$RSOCK"
}

run3med() {
  local op=$1 c=$2 p=$3 n=$4 sock=$5
  declare -a s=()
  for _ in 1 2 3; do
    v=$(timeout 60 taskset -c 10-13 "$RB" -s "$sock" -t "${op,,}" -c $c -n $n -P $p -q 2>&1 | tr '\r' '\n' | grep -E "^${op}: [0-9]" | tail -1 | awk '{print $2}')
    s+=("${v:-0}")
  done
  printf '%s\n' "${s[@]}" | sort -n | awk 'NR==2{print}'
}

echo "=== UDS bench (3-run median, kevy vs valkey vs redis) ==="
for srv in kevy valkey redis; do
  start_${srv}
  case $srv in
    kevy) sock=$SOCK ;;
    valkey) sock=$VSOCK ;;
    redis) sock=$RSOCK ;;
  esac
  c1s=$(run3med SET 1 1 200000 "$sock")
  c1g=$(run3med GET 1 1 200000 "$sock")
  c50s=$(run3med SET 50 1 1000000 "$sock")
  c50g=$(run3med GET 50 1 1000000 "$sock")
  c50p16s=$(run3med SET 50 16 1000000 "$sock")
  c100s=$(run3med SET 100 1 1000000 "$sock")
  printf "%s  c1=%s/%s  c50-P1=%s/%s  c50-P16=%s  c100-P1=%s\n" "$srv" "$c1s" "$c1g" "$c50s" "$c50g" "$c50p16s" "$c100s"
done
kill_all
