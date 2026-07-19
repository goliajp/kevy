# INCIDENT — `perfgate.sh` killed the entire host, three times, as root

**Severity: catastrophic.** A *benchmark script* took a production-adjacent
server (`lx64`) completely offline on **three separate days** — 2026-07-12,
07-17, 07-18 — by sending `SIGTERM` to **every process on the box**, including
`sshd`, `tailscaled`, `systemd` (PID 1), `dockerd`, `dnsmasq`, and `dhcpcd`.
Each time the machine went dark to the network and required a **physical visit
to the office** to recover. It cost days of misdiagnosis (it was blamed on a
flaky PSU, thermal throttling, kernel panics, and even "someone typed a bad
`pkill` by hand") before the actual cause was found: **our own bench harness.**

This is not a subtle bug. This is a bench script that was one empty variable away
from `kill -TERM` on the whole operating system, with **nothing** standing in the
way, running **as root**, for who knows how long. Let's be honest about how many
independent mistakes had to line up, because every single one of them was
avoidable and several are embarrassing.

---

## The one line that did it

```sh
server_stop() {
  pkill -f "^$BIN" 2>/dev/null      # <-- when $BIN is empty this is: pkill -f "^"
  while pgrep -f "^$BIN" >/dev/null; do sleep 0.1; done
}
```

`pkill -f "^"` is `pkill -f <a regex that matches every possible command line>`.
Run as root it is functionally identical to `killall5` / `kill -TERM -1`: it
SIGTERMs the entire process table. A benchmark's "stop the server I started"
routine was written such that **one empty string turns it into a machine-wide
kill switch.**

## How `$BIN` became empty (the part that is genuinely nasty)

```sh
REF_BIN=${PERFGATE_REF_BIN:-$(ref_binary "$REF_SHA")}   # ref_binary refuses on build failure
...
measure_all ref "$REF_BIN"                              # $REF_BIN is ""
...
measure_all() { local who=$1 ...; BIN=$2; server_start ...; }   # BIN=""  ->  server_stop  ->  pkill -f "^"
```

`ref_binary` "handles" a failed reference build like this:

```sh
ref_binary() {
  ...
  ( cd "$wt" && cargo build ... ) || { ...; refuse "reference build failed at $sha"; }
  ...
}
refuse() { echo "perfgate: REFUSED — $1" >&2; exit 2; }
```

`refuse` calls `exit 2` and the author clearly believed that aborts the run.
**It does not.** `ref_binary` is invoked inside a command substitution
`REF_BIN=$(ref_binary …)`, and `exit` inside `$(…)` **only exits the subshell.**
The parent script sails right past it with `REF_BIN=""`, hands the empty string
to `measure_all`, which assigns `BIN=""`, which reaches `server_stop`, which
fires `pkill -f "^"`. The "guard" was a `exit` that guarded nothing.

So the trigger is simply: **the baseline reference binary fails to build.** A bad
baseline SHA, a toolchain that moved on, a transient `cargo`/`git` hiccup, a full
disk — any of these turns a routine perf-gate run into "SIGTERM the whole host".
That is why it was intermittent (3 times, not every run) and why it looked like
flaky hardware.

---

## Every failure that had to happen — and every guard that wasn't there

This incident is a stack of independent mistakes. Each one alone would have
prevented the outage. **All** of them were missing:

1. **`pkill -f` with an interpolated variable and no emptiness check.** The
   single most important rule of `pkill -f "$X"` is *never let `$X` be empty*,
   because empty → matches everything. There was no `[ -n "$BIN" ]`. Not one
   line. The repo's own SOP (documented right there in `v125-precision.sh`:
   "`pkill -x` NOT `-f`, per the zombie-incident lesson") existed precisely
   because `pkill -f` had *already bitten this project once* — and `perfgate.sh`
   ignored it and used `-f` anyway, with a variable, with no anchor guard.

2. **`^$BIN` — the worst possible pattern to leave unguarded.** Of all the ways
   an empty variable could fail, `"^$BIN"` collapses to `"^"`, which is not "match
   nothing" (that might have been survivable) — it is "match *everything*". An
   empty `-x` pattern matches nothing; an empty `-f "^…"` pattern matches the
   universe. The most dangerous choice was made by accident.

3. **`exit` inside `$(…)` believed to abort the parent.** A classic,
   well-known shell trap. `set -euo pipefail` was *on* (`set -u` is at the top of
   the file) and it bought nothing here, because `set -u` catches *unset*
   variables, not *empty* ones, and command substitution swallows the subshell's
   exit code into a string assignment. Defensive flags create false confidence
   when you don't understand what they actually check.

4. **No validation of `$REF_BIN` after building it.** `REF_BIN=$(…)` and then
   straight into `measure_all` with zero `[ -x "$REF_BIN" ]` check. The one place
   the empty value was born, and it was passed on unexamined.

5. **`BIN` reassigned inside a function (`measure_all`: `BIN=$2`), defeating the
   only real guard in the file.** The top of the script has
   `BIN=${1:?usage…}` — a proper guard. Then `measure_all` quietly reassigns the
   same global `BIN` from `$2` with no guard, so the one correct check is bypassed
   for the code path that actually calls `server_stop`. (This is also why a
   grep-level audit wrongly cleared the script as "safe, guarded by `${1:?}`" —
   the reassignment is easy to miss and it matters more than the declaration.)

6. **Running the whole thing as root.** A userland micro-benchmark comparing two
   builds of a KV store has no business running as root. It ran as root on `lx64`
   (multiple checkouts under `/root/kevy*`). A non-root user's `pkill -f "^"`
   would have massacred *that user's* processes and stopped — annoying, local,
   recoverable. As root it took out `sshd`, `systemd`, and the network stack. The
   privilege turned a self-inflicted foot-gun into a site outage.

7. **No blast-radius limiting on a kill.** There are half a dozen ways to stop
   "the server I started" that *cannot* escalate to the whole host: kill the
   captured `$SRV` PID (the script *has* `SRV=$!` right there!), `pkill -x` on an
   exact binary name, `setsid` + kill the process group, `--pidfile`. The script
   already captures `$SRV` and then throws it away in favor of a fuzzy
   `pkill -f`. The safe handle was in hand and discarded.

8. **The signal was `SIGTERM`, so nothing screamed.** `pkill -f` defaults to
   SIGTERM, and systemd treats SIGTERM to a service as a *clean* exit. So the
   daemons died "successfully", `Restart=on-failure` declined to restart them
   (SIGTERM isn't a failure), and there was no crash, no core, no `pstore`, no
   alert — just a silently dark machine. A catastrophic bug that leaves *zero*
   forensic trace by design.

Eight failures. Any one fixed would have avoided every outage. The bench harness
had the safety culture of a `rm -rf "$DIR/"` with an unset `$DIR`.

---

## Blast radius

Per incident, a single `pkill -f "^"` as root SIGTERM'd (confirmed in the `lx64`
journal): `systemd[1]`, `systemd-journald`, `sshd`, `tailscaled`, `dockerd`,
`dnsmasq`, `dhcpcd`, `smartd`, plus every app on the box (an unrelated
`insight-server`, the CI runner, etc.). Network access (`tailscaled` + `dhcpcd` +
`dnsmasq`) and remote login (`sshd`) all died together, so the box was
unreachable while still "up" — the worst failure mode: looks alive, answers
nothing, needs hands on a keyboard in the office.

Three outages: **2026-07-12 17:00, 2026-07-17 19:09, 2026-07-18 02:29.**

## The fix

`bench/perfgate.sh`, commit `7a87a1aa`:

```sh
server_stop() {
  # HARD GUARD: empty $BIN makes "^$BIN" == "^", which pkill -f matches against
  # EVERY process — as root that SIGTERMs sshd/systemd/the whole box.
  [ -n "$BIN" ] || { echo "perfgate: server_stop refusing pkill with empty BIN" >&2; return 0; }
  pkill -f "^$BIN" 2>/dev/null
  ...
}
```

and, at the source, catch the subshell-exit-doesn't-propagate case:

```sh
REF_BIN=${PERFGATE_REF_BIN:-$(ref_binary "$REF_SHA")}
[ -n "$REF_BIN" ] && [ -x "$REF_BIN" ] || fail "reference binary unavailable for ${REF_SHA:0:12} (build failed?) — aborting before it can nuke the box"
```

The four live checkouts on `lx64` (`kevy-ci-repro`, `kevy-dev`, `kevy-k307`,
`kevy-mmap`) were patched in place with the same `server_stop` guard.

---

## Hard rules for every script in `bench/` (non-negotiable)

1. **Never `pkill`/`kill` on an interpolated pattern without an emptiness guard
   on the immediately-preceding line.** `[ -n "$X" ] || return`/`|| exit`. No
   exceptions. `set -u` does **not** cover this.
2. **Never `pkill -f "^$X"` or any pattern that becomes match-all when `$X` is
   empty.** Prefer `pkill -x <exact-name>` (already the documented repo SOP) or
   kill a captured PID / process group. If you already captured the PID
   (`SRV=$!`), *use it* — do not re-find it fuzzily.
3. **`exit` inside `$(…)` does not abort the parent.** If a helper called via
   command substitution can fail, validate its output in the parent
   (`[ -x "$OUT" ] || fail`). Do not rely on `refuse`/`exit` inside `$()`.
4. **Bench scripts do not run as root.** If a bench needs root, that is a design
   smell — fix the need, don't paper over it with privilege. A perf comparison of
   two userland binaries never legitimately needs to be able to SIGTERM `init`.
5. **A kill in a bench must be blast-radius-bounded by construction** — exact
   name, captured PID, or process group — such that even with every variable
   empty it cannot reach a process the script did not itself start.

This class of bug — "cleanup routine escalates to host-wide kill because a
variable went empty" — has now cost this project two documented incidents (the
earlier `-f`→`-x` "zombie incident", and this one). The lesson was written down
after the first and ignored in `perfgate.sh`. Write it down again, and this time
grep the whole `bench/` tree for `pkill -f "\$` and `pkill -f "^` before shipping.
