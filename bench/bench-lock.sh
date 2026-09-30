# Sourced at the top of a benchmark entry point, before it does anything:
# re-runs the calling script under the machine's bench lock, held until the
# script exits.
#
#   . "$(dirname "$0")/bench-lock.sh"
#
# The lock is machine-wide and shared with other projects' benchmarks and
# with heavy CI jobs, which wait for it before building. It only keeps
# those jobs from starting in the middle of a measurement; it says nothing
# about whether the box was quiet, which is what each script's own
# preflight checks are for.
#
#   macOS  /Users/Shared/bench.lock   /usr/bin/lockf (macOS has no flock)
#   Linux  /var/lock/bench.lock       flock
#
# KEVY_BENCH_LOCK overrides the path. A script run by another that already
# holds the lock (perfgate-median running perfgate, say) does not take it
# again: the environment says it is held.
if [ -z "${KEVY_BENCH_LOCK_HELD:-}" ]; then
  export KEVY_BENCH_LOCK_HELD=1
  case "$(uname -s)" in
    Darwin)
      _bench_lock=${KEVY_BENCH_LOCK:-/Users/Shared/bench.lock}
      /usr/bin/lockf -k -t 0 "$_bench_lock" true 2>/dev/null \
        || echo "bench lock $_bench_lock is held — waiting for it" >&2
      exec /usr/bin/lockf -k "$_bench_lock" "$BASH" "$0" "$@"
      ;;
    Linux)
      _bench_lock=${KEVY_BENCH_LOCK:-/var/lock/bench.lock}
      flock -n "$_bench_lock" true 2>/dev/null \
        || echo "bench lock $_bench_lock is held — waiting for it" >&2
      exec flock "$_bench_lock" "$BASH" "$0" "$@"
      ;;
  esac
fi
