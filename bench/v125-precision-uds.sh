#!/bin/bash
# v1.25 precision bench harness — UDS variant (mirrors v125-precision.sh).
#
# Same n=1M × 10 runs, 95% CI agg shape; uses Unix-domain socket on
# kevy/valkey/redis to skip the TCP loopback floor. Per user observation
# (2026-06-22) the TCP precision bench showed kevy tied with valkey at
# c=50/100 -P 1 because both saturated TCP RTT; UDS removes that floor
# and lets server-CPU efficiency surface.
#
# kevy listens on a Unix socket via KEVY_UNIX_SOCKET env var (introduced
# in commit 5134b34). valkey via `--unixsocket` config. redis same.
#
# Usage: bash bench/v125-precision-uds.sh [OUT_FILE]
set -u
KBIN=${KEVY_BIN:-/root/kevy/target/release/kevy}
VBIN=${VALKEY_BIN:-/root/srcbench/valkey/src/valkey-server}
RBIN=${REDIS_BIN:-/root/srcbench/redis/src/redis-server}
RB=${REDIS_BENCH:-/root/srcbench/redis/src/redis-benchmark}
RUNS=${RUNS:-10}
N=${N:-1000000}
OUT=${1:-/tmp/v125-precision-uds.md}

KEVY_SOCK=/tmp/kevy.sock
VALKEY_SOCK=/tmp/valkey.sock
REDIS_SOCK=/tmp/redis.sock

ulimit -n 200000

kill_all() {
  pkill -9 -x kevy 2>/dev/null
  pkill -9 -x valkey-server 2>/dev/null
  pkill -9 -x redis-server 2>/dev/null
  rm -f "$KEVY_SOCK" "$VALKEY_SOCK" "$REDIS_SOCK"
  sleep 2
}

wait_ready_uds() {
  local sock=$1
  for _ in $(seq 1 30); do
    "$RB" -s "$sock" -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy() {
  kill_all
  # kevy still needs a (dummy) TCP port even when serving UDS — the runtime
  # always binds TCP. Use a free port that won't conflict with valkey/redis.
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 KEVY_UNIX_SOCKET="$KEVY_SOCK" \
    taskset -c 0 "$KBIN" --threads 1 --port 7099 --no-aof \
    </dev/null >/tmp/k.log 2>&1 &
  disown
  sleep 1.5
  wait_ready_uds "$KEVY_SOCK"
}

start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port 0 --unixsocket "$VALKEY_SOCK" \
    --unixsocketperm 777 --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes \
    </dev/null >/tmp/v.log 2>&1 &
  disown
  sleep 1.5
  wait_ready_uds "$VALKEY_SOCK"
}

start_redis() {
  kill_all
  setsid taskset -c 0-9 "$RBIN" --port 0 --unixsocket "$REDIS_SOCK" \
    --unixsocketperm 777 --bind 127.0.0.1 --protected-mode no \
    --save '' --appendonly no --io-threads 10 --io-threads-do-reads yes \
    </dev/null >/tmp/r.log 2>&1 &
  disown
  sleep 1.5
  wait_ready_uds "$REDIS_SOCK"
}

# Run `redis-benchmark` RUNS times for one scenario over UDS; emit raw rps.
# Args: op, c, p, sock_path.
run_raw() {
  local op=$1 c=$2 p=$3 sock=$4
  local out=$(mktemp)
  for _ in $(seq 1 $RUNS); do
    timeout 90 taskset -c 10-13 "$RB" -s "$sock" -t "${op,,}" -c "$c" -n $N -P "$p" -q 2>&1 \
      | tr '\r' '\n' \
      | grep -E "^${op}: [0-9]" \
      | tail -1 \
      | awk '{print $2}' \
      >> "$out"
  done
  echo "$out"
}

agg() {
  local file=$1
  awk '
    { n++; v[n] = $1; sum += $1 }
    END {
      if (n == 0) { print "mean=NA std=NA ci95=NA mean_filt=NA n=0 filt_n=0"; exit }
      mean = sum / n
      ssq = 0
      for (i = 1; i <= n; i++) ssq += (v[i] - mean) * (v[i] - mean)
      std = (n > 1) ? sqrt(ssq / (n - 1)) : 0
      ci95 = (n > 1) ? 1.96 * std / sqrt(n) : 0
      f_sum = 0; f_n = 0
      for (i = 1; i <= n; i++) {
        if (v[i] >= mean - 2*std && v[i] <= mean + 2*std) {
          f_sum += v[i]; f_n++
        }
      }
      mean_filt = (f_n > 0) ? f_sum / f_n : mean
      printf "mean=%.1f std=%.1f ci95=%.1f mean_filt=%.1f n=%d filt_n=%d\n",
        mean, std, ci95, mean_filt, n, f_n
    }
  ' "$file"
}

declare -a SCENARIOS=(
  "c1-P1   SET 1   1"
  "c1-P1   GET 1   1"
  "c50-P1  SET 50  1"
  "c50-P1  GET 50  1"
  "c50-P16 SET 50  16"
  "c50-P16 GET 50  16"
  "c100-P1 SET 100 1"
  "c100-P1 GET 100 1"
)

declare -A SOCKS=(
  [kevy]=$KEVY_SOCK
  [valkey]=$VALKEY_SOCK
)

mkdir -p "$(dirname "$OUT")"
{
  echo "# v1.25 precision bench — UDS (n=$N × runs=$RUNS, 1.96σ CI95)"
  echo
  echo "Host: $(hostname)  Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "Transport: Unix-domain stream socket (no TCP loopback)"
  echo "kevy: --threads 1 + KEVY_UNIX_SOCKET=$KEVY_SOCK, taskset 0"
  echo "valkey: --unixsocket $VALKEY_SOCK --io-threads 10, taskset 0-9"
  echo "Client: taskset 10-13, redis-benchmark 8.8 -s <sock>"
  echo
  printf "| %-9s | %-3s | %-7s | %-12s | %-10s | %-10s | %-12s | %-10s |\n" \
    "scenario" "op" "server" "mean(rps)" "std" "ci95(±)" "mean(2σ-filt)" "filt n"
  printf "|---|---|---|---:|---:|---:|---:|---:|\n"

  for spec in "${SCENARIOS[@]}"; do
    read label op c p <<<"$spec"
    for srv in kevy valkey; do
      "start_${srv}" || { echo "  $srv start FAIL"; continue; }
      sock=${SOCKS[$srv]}
      raw=$(run_raw "$op" "$c" "$p" "$sock")
      stats=$(agg "$raw")
      eval "$stats"
      printf "| %-9s | %-3s | %-7s | %12.1f | %10.1f | %10.1f | %12.1f | %10d |\n" \
        "$label" "$op" "$srv" "$mean" "$std" "$ci95" "$mean_filt" "$filt_n"
      rm -f "$raw"
    done
  done

  kill_all
  echo
  echo "## Cross-server ratios (kevy mean_filt / valkey mean_filt)"
  echo
  echo "(see above table)"
} | tee "$OUT"

echo
echo "Written: $OUT"
