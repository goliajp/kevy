#!/bin/bash
# v1.25 precision bench harness — per .claude/rule/perf-vs-foss.md R7.
#
# Many v1.25 attacks measured "sub-noise" (within ±5% variance band on
# lx64 with shared host load). To validate <5 µs / <5 ns wins claimed
# by Phase A decomps, we need higher-N + multi-run aggregation:
#   - n=1M per single bench (vs std 100k)
#   - 10 runs per scenario (vs std 3)
#   - aggregate: mean, std, 95% CI (1.96σ/√N), outlier-rejected mean
#
# Per-bench timeout 90 s (V125-AXIS-K backlog-polish-trap lesson).
# pkill -9 -x kevy (NOT -f) between server switches (zombie-incident
# memory: feedback-remote-bench-zombie-incident).
#
# Usage: bash bench/v125-precision.sh [OUT_FILE]
#   defaults to /tmp/v125-precision-$(date +%s).md

set -u
KBIN=${KEVY_BIN:-/root/kevy/target/release/kevy}
VBIN=${VALKEY_BIN:-/root/srcbench/valkey/src/valkey-server}
RB=${REDIS_BENCH:-/root/srcbench/redis/src/redis-benchmark}
PORT=${PORT:-7001}
RUNS=${RUNS:-10}
N=${N:-1000000}
OUT=${1:-/tmp/v125-precision.md}

ulimit -n 200000

kill_all() {
  pkill -9 -x kevy 2>/dev/null
  pkill -9 -x valkey-server 2>/dev/null
  sleep 2
}

wait_ready() {
  for _ in $(seq 1 30); do
    "$RB" -h 127.0.0.1 -p $PORT -t ping -n 1 -q >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_kevy() {
  kill_all
  setsid env KEVY_IO_URING=1 KEVY_BIND=127.0.0.1 taskset -c 0 \
    "$KBIN" --threads 1 --port $PORT --no-aof </dev/null >/tmp/k.log 2>&1 &
  disown
  sleep 1
  wait_ready
}

start_valkey() {
  kill_all
  setsid taskset -c 0-9 "$VBIN" --port $PORT --bind 127.0.0.1 \
    --protected-mode no --save '' --appendonly no \
    --io-threads 10 --io-threads-do-reads yes </dev/null >/tmp/v.log 2>&1 &
  disown
  sleep 1.5
  wait_ready
}

# Run `redis-benchmark` RUNS times for one scenario; emit raw rps newline-
# separated. Args: op, c, p, d (0 for default-size). Result: file path.
run_raw() {
  local op=$1 c=$2 p=$3 d=$4
  local out=$(mktemp)
  local args=(-h 127.0.0.1 -p $PORT -t "${op,,}" -c "$c" -n $N -P "$p" -q)
  [ "$d" != "0" ] && args+=(-d "$d")
  for _ in $(seq 1 $RUNS); do
    timeout 90 taskset -c 10-13 "$RB" "${args[@]}" 2>&1 \
      | tr '\r' '\n' \
      | grep -E "^${op}: [0-9]" \
      | tail -1 \
      | awk '{print $2}' \
      >> "$out"
  done
  echo "$out"
}

# Aggregate raw rps into mean / std / 95% CI / outlier-rejected mean.
# Args: rawfile. Emits: "mean=$MEAN std=$STD ci95=$CI95 mean_filt=$MEAN_FILT".
agg() {
  local file=$1
  awk '
    {
      n++
      v[n] = $1
      sum += $1
    }
    END {
      if (n == 0) { print "mean=NA std=NA ci95=NA mean_filt=NA"; exit }
      mean = sum / n
      ssq = 0
      for (i = 1; i <= n; i++) ssq += (v[i] - mean) * (v[i] - mean)
      std = (n > 1) ? sqrt(ssq / (n - 1)) : 0
      ci95 = (n > 1) ? 1.96 * std / sqrt(n) : 0
      # 2σ outlier rejection
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

# Scenarios — focus where Phase A claimed sub-noise wins (5-8 ns / op).
declare -a SCENARIOS=(
  "c1-P1   SET 1   1  0"
  "c1-P1   GET 1   1  0"
  "c50-P1  SET 50  1  0"
  "c50-P1  GET 50  1  0"
  "c50-P16 SET 50  16 0"
  "c50-P16 GET 50  16 0"
  "c100-P1 SET 100 1  0"
  "c100-P1 GET 100 1  0"
)

declare -a SERVERS=(kevy valkey)

mkdir -p "$(dirname "$OUT")"
{
  echo "# v1.25 precision bench (n=$N × runs=$RUNS, 1.96σ CI95)"
  echo
  echo "Host: $(hostname)  Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "kevy: --threads 1, taskset 0  |  valkey: --io-threads 10, taskset 0-9"
  echo "Client: taskset 10-13, redis-benchmark 8.8"
  echo
  printf "| %-9s | %-3s | %-7s | %-12s | %-10s | %-10s | %-12s | %-10s |\n" \
    "scenario" "op" "server" "mean(rps)" "std" "ci95(±)" "mean(2σ-filt)" "filt n"
  printf "|---|---|---|---:|---:|---:|---:|---:|\n"

  for spec in "${SCENARIOS[@]}"; do
    read label op c p d <<<"$spec"
    for srv in "${SERVERS[@]}"; do
      "start_${srv}" || { echo "  $srv start FAIL"; continue; }
      raw=$(run_raw "$op" "$c" "$p" "$d")
      stats=$(agg "$raw")
      eval "$stats"  # exports mean, std, ci95, mean_filt, n, filt_n
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
