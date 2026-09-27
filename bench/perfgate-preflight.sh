# Sourced by perfgate.sh before anything is started. Sets RUNDIR and the
# EXIT trap that keeps it on a failed run.

# ---------- preflight: never measure on a dirty box ----------
# A perf comparison of two userland binaries never legitimately needs the
# ability to SIGTERM init. Running this as root is what turned a foot-gun
# into three site outages (
# hard rule 4) — the privilege is the difference between "killed my own
# processes" and "killed sshd". No override flag: an escape hatch here would
# be used, and then it would be the default again.
[ "$(id -u)" -ne 0 ] || refuse "refusing to run as root — use an unprivileged \
bench account (lx64: kevybench, checkout ~/kevy)"
# Per-run scratch. A fixed /tmp/perfgate_* path is unwritable the moment a
# different account ran the gate before you — which is exactly what the
# root-to-kevybench migration produced.
RUNDIR=$(mktemp -d "${TMPDIR:-/tmp}/perfgate-XXXXXX")
# Kept on a failed/refused run — the server log in there is the only
# evidence of why it did not come up.
on_exit() {
  local rc=$?
  if [ $rc -eq 0 ]; then rm -rf "$RUNDIR"
  else echo "perfgate: scratch kept at $RUNDIR" >&2; fi
}
trap on_exit EXIT
command -v redis-benchmark >/dev/null || refuse "redis-benchmark not installed"
command -v redis-cli >/dev/null || refuse "redis-cli not installed (the --threads angles read the server's counter through it)"
[ -x "$BIN" ] || refuse "$BIN is not executable"
# The sweep pattern "kevy" also matches the bench ACCOUNT NAME inside
# a driver's `sudo -u kevybench …` cmdline — a wrapper script that
# never touched a server would be refused as a leftover. Exclude the
# sudo wrapper line itself; real leftover servers/benchmarks are
# direct processes, not sudo shells.
# …and exclude this gate's own ANCESTRY. A runner that invokes the gate
# (tools/suite.py, a wrapper shell, nohup) has "kevy" in its cmdline by
# way of the repo path, and the name-based excludes above cannot know
# every runner's name — the suite's first box run was refused because
# the sweep matched the suite itself. Real leftovers are never our own
# ancestors.
ANCESTORS=""
APID=$$
while [ "$APID" -gt 1 ] 2>/dev/null; do
  ANCESTORS="$ANCESTORS|^$APID "
  APID=$(awk '{print $4}' "/proc/$APID/stat" 2>/dev/null || echo 1)
done
LEFTOVER=$(pgrep -af "kevy|redis-benchmark" | grep -Ev "${ANCESTORS#|}" \
  | grep -v perfgate | grep -v claude | grep -v kevypgcmp | grep -v suite.py \
  | grep -v "sudo -u kevybench" || true)
[ -n "$LEFTOVER" ] && refuse "leftover bench processes (sweep first):
$LEFTOVER"
# Instantaneous idle%, not 1-min loadavg: loadavg measures the past, so a
# back-to-back run (baseline then gate) would refuse on its own wake. Two
# /proc/stat samples 1s apart = what the box is doing RIGHT NOW.
read -r _ u1 n1 s1 i1 _ < /proc/stat; sleep 1; read -r _ u2 n2 s2 i2 _ < /proc/stat
IDLE=$(( (i2 - i1) * 100 / ( (u2-u1) + (n2-n1) + (s2-s1) + (i2-i1) ) ))
[ "$IDLE" -ge 80 ] || refuse "box busy (idle ${IDLE}% < 80%)"
