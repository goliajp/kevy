#!/usr/bin/env python3
"""One perfgate2 measurement window, read on the box.

  perfgate2_window.py --baseline F --angle A --side ref|cand --obs N --win N \
      --srv-pid P --srv-cpus 0-3 --port 7001 --secs 3 [--gens pid:threads,...] \
      [--lines all|T] [-- latency command...]

Reads per-CPU /proc/stat, the server's and each generator's utime+stime, the
server's command counter, and the server's hardware counters (through
kevy-perfstat) across one window, and prints the window as one JSON line.
With a latency command instead of --gens, the window is that command's run
and its stdout (whole µs) is the value.

Exit: 0 keep, 10 discard (reason on stderr, the line is still printed),
2 refused (server died, counters unavailable, counter unreadable).
"""

import argparse
import json
import os
import pathlib
import socket
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "tools"))
import perfwindow as pw  # noqa: E402

PERFSTAT = ["sudo", "-n", "/usr/local/sbin/kevy-perfstat"]
COUNTER_FIX = """hardware counters unavailable to this account. Fix (root, once):
  install /usr/local/sbin/kevy-perfstat with a sudoers rule for the bench account
  (perf stat -x, -e instructions:u,instructions:k,cycles,task-clock,raw_syscalls:sys_enter -p PID -- sleep SECS)
or give a perf binary cap_perfmon. PERFGATE_LINES=T judges throughput only,
and that run cannot judge a 3% change."""


def refuse(msg):
    print(f"perfgate2: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def proc_ticks(pid):
    f = pathlib.Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(f[11]) + int(f[12])  # utime, stime (fields 14, 15)


def total_commands(port):
    # a saturated shard can starve a fresh connection; retry, never hang
    for _ in range(3):
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                s.sendall(b"*2\r\n$4\r\nINFO\r\n$5\r\nstats\r\n")
                buf = b""
                while b"total_commands_processed:" not in buf or not buf.endswith(b"\r\n"):
                    chunk = s.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
            for line in buf.decode(errors="replace").split("\r\n"):
                if line.startswith("total_commands_processed:"):
                    return int(line.split(":", 1)[1])
        except OSError:
            continue
    return None


def gen_list(spec):
    return [tuple(int(x) for x in g.split(":")) for g in spec.split(",") if g]


def snapshot(a, gens):
    return (pw.parse_proc_stat(pathlib.Path("/proc/stat").read_text()),
            proc_ticks(a.srv_pid), [proc_ticks(p) for p, _ in gens])


def measure(a, gens):
    """The window body: returns (perf dict or None, value_us or None)."""
    if a.latency:
        out = subprocess.run(a.latency, capture_output=True, text=True)
        if out.returncode != 0 or not out.stdout.strip().isdigit():
            refuse(f"latency run failed: {out.stderr.strip()[-200:]}")
        return None, int(out.stdout.strip())
    if a.lines == "T":
        time.sleep(a.secs)
        return None, None
    run = subprocess.run(PERFSTAT + [str(a.srv_pid), str(a.secs)],
                         capture_output=True, text=True)
    if run.returncode == 2:
        refuse(f"the server (pid {a.srv_pid}) died during the window")
    perf = pw.parse_perfstat(run.stdout) if run.returncode == 0 else None
    if perf is None:
        refuse(COUNTER_FIX + f"\n(kevy-perfstat exit {run.returncode}: "
               f"{(run.stderr or run.stdout).strip()[-300:]})")
    return perf, None


def main():
    ap = argparse.ArgumentParser()
    for k in ("baseline", "angle", "side", "srv-cpus", "lines"):
        ap.add_argument("--" + k, default="all" if k == "lines" else None)
    for k in ("obs", "win", "srv-pid", "port", "secs"):
        ap.add_argument("--" + k, type=int)
    ap.add_argument("--gens", default="")
    ap.add_argument("latency", nargs="*")
    a = ap.parse_args()
    topo = json.loads(pathlib.Path(a.baseline).read_text())["topology"]
    gens = gen_list(a.gens)
    try:
        p0, s0, g0 = snapshot(a, gens)
    except FileNotFoundError as e:
        refuse(f"a process of this window is gone before it started ({e.filename})")
    c0, t0 = total_commands(a.port), time.monotonic_ns()
    perf, value = measure(a, gens)
    c1, t1 = total_commands(a.port), time.monotonic_ns()
    try:
        p1, s1, g1 = snapshot(a, gens)
    except FileNotFoundError as e:
        refuse(f"a process of this window exited inside it ({e.filename}) — "
               "a generator ran out of requests or the server died")
    w = {"angle": a.angle, "side": a.side, "obs": a.obs, "win": a.win,
         "srv_cpus": a.srv_cpus, "secs": a.secs, "hz": os.sysconf("SC_CLK_TCK"),
         "cpu": pw.proc_stat_delta(p0, p1), "srv_ticks": s1 - s0,
         "gens": [[b - x, g[1]] for x, b, g in zip(g0, g1, gens)], "perf": perf}
    if value is not None:
        w["value_us"] = value
    else:
        if c0 is None or c1 is None or c1 <= c0:
            refuse(f"INFO total_commands_processed unreadable or flat on port {a.port}")
        w["cmds"], w["wall_ns"] = c1 - c0, t1 - t0
    v = pw.classify(w, topo)
    w["classify"] = v
    print(json.dumps(w, sort_keys=True))
    if v["discard"]:
        print(f"perfgate2: discard {a.angle} {a.side}: {v['discard']}", file=sys.stderr)
        sys.exit(10)


if __name__ == "__main__":
    main()
