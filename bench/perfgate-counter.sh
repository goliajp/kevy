# Sourced by perfgate.sh: throughput read off the server's own command
# counter, for the angles where redis-benchmark's figure is quantized.

# The server's own view of how much work it did. `--threads` makes
# redis-benchmark's reported rps a quantized, understated number (see the
# header); the command counter does not lie about that. Two INFO calls per
# measurement are lost in the noise of tens of millions of ops.
#
# The timeout is not decoration. A shard saturated by a heavy workload can
# leave a fresh connection's INFO queued for a very long time, and an
# unbounded read here parks the whole gate behind it — which is exactly how
# the first run of this harness hung for an hour.
srv_cmds() {
  # Up to 3 probes: a heavy pipelined angle (zalg's ZINTERSTORE at
  # -P 16) can starve a fresh INFO connection past one 5s timeout —
  # observed twice (2026-08-02), both times only on that angle. The
  # counter window stays sound under retries because the caller takes
  # its timestamp AFTER the read returns; a retried probe is printed
  # so the starvation stays visible instead of vanishing into a pass.
  local n try
  for try in 1 2 3; do
    n=$(timeout 5 redis-cli -p 7001 INFO stats 2>/dev/null | tr -d '\r' \
      | awk -F: '/^total_commands_processed:/ {print $2}')
    if [ -n "$n" ]; then
      [ "$try" -gt 1 ] \
        && echo "perfgate: INFO answered on probe $try — heavy-angle starvation" >&2
      echo "$n"
      return 0
    fi
  done
  return 0
}

# Drive the load, then read the server counter across a window we time
# ourselves. The load generator is left running for the whole window and
# killed afterwards, so N only has to be large enough to outlast RAMP +
# WINDOW at THIS angle's rate — it is not the unit of measurement any more.
# The generator is killed before the samples are judged, so a failed read
# cannot leave a 60M-request benchmark running behind the gate.
steady_rps() { # $1... = the redis-benchmark argv after the pinning
  local bpid c0 t0 c1 t1
  taskset -c 8-15 "$@" >/dev/null 2>&1 &
  bpid=$!
  sleep "$RAMP"
  c0=$(srv_cmds); t0=$(date +%s%N)
  sleep "$WINDOW"
  c1=$(srv_cmds); t1=$(date +%s%N)
  kill "$bpid" 2>/dev/null
  wait "$bpid" 2>/dev/null
  # This runs inside a command substitution, so `refuse` here would exit only
  # the subshell and the caller would carry on with an empty sample. Emit 0
  # instead and let the measure loop refuse on it — an unmeasured angle must
  # stop the gate, never pass quietly.
  if [ -z "$c0" ] || [ -z "$c1" ]; then
    echo "perfgate: INFO stats unreadable during '$*' — is a shard wedged?" >&2
    printf "0"
    return
  fi
  awk -v c0="$c0" -v c1="$c1" -v t0="$t0" -v t1="$t1" \
    'BEGIN {printf "%.0f", (c1 - c0) / ((t1 - t0) / 1e9)}'
}
