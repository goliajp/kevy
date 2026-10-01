#!/usr/bin/env python3
"""perfgate's measurement: one window of a running server, and the box.

  perfgate_measure.py window --srv-pid P --srv-cpus 0-3 --port N --secs 3 \
      [--gens pid:threads,...] [--angle A --side S --obs N --win N]

prints one window as a JSON line (bench/arena.sh uses this). Everything in
it is a delta across the window: per-CPU /proc/stat, the server's and each
generator's utime+stime, the server's command counter, and the server's
hardware counters from `perf stat -p`.

Counters come from `perf stat` directly when this runs as root, and from
`sudo -n /usr/local/sbin/kevy-perfstat PID SECS` otherwise (the bench
account's helper on lx64: perf stat on one pid it owns, fixed events).
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
import perfreport as pr  # noqa: E402

HELPER = "/usr/local/sbin/kevy-perfstat"


class Broken(Exception):
    """The measurement could not be taken; the message says why."""


def proc_ticks(pid):
    f = pathlib.Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(f[11]) + int(f[12])  # utime, stime (fields 14, 15)


def rss_mb(pid):
    for line in pathlib.Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) / 1024
    return None


def box_stat():
    return pr.parse_proc_stat(pathlib.Path("/proc/stat").read_text())


def info_field(port, field, section="stats", tries=3):
    """One INFO field over a raw socket. A saturated shard can starve a fresh
    connection, so a read is retried on timeout instead of waited on."""
    req = f"*2\r\n$4\r\nINFO\r\n${len(section)}\r\n{section}\r\n".encode()
    for _ in range(tries):
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                s.sendall(req)
                f = s.makefile("rb")
                hdr = f.readline()
                if not hdr.startswith(b"$"):
                    continue
                body = f.read(int(hdr[1:]))
            for line in body.decode(errors="replace").split("\r\n"):
                if line.startswith(f"{field}:"):
                    return line.split(":", 1)[1]
        except (OSError, ValueError):
            continue
    return None


def ping(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=1) as s:
            s.sendall(b"*1\r\n$4\r\nPING\r\n")
            return s.recv(16).startswith(b"+PONG")
    except OSError:
        return False


def counters(pid, secs):
    """perf stat on the server for secs whole seconds -> {event: value}."""
    if os.geteuid() == 0 or not os.path.exists(HELPER):
        run = subprocess.run(["perf", "stat", "-x,", "-e", ",".join(pr.PERF_EVENTS),
                              "-p", str(pid), "--", "sleep", str(secs)],
                             capture_output=True, text=True)
        text = run.stderr
        if "raw_syscalls" in text and "event syntax error" in text:
            # a box without tracefs: count the rest, sys/op stays empty
            run = subprocess.run(["perf", "stat", "-x,", "-e",
                                  ",".join(e for e in pr.PERF_EVENTS if e not in pr.OPTIONAL_EVENTS),
                                  "-p", str(pid), "--", "sleep", str(secs)],
                                 capture_output=True, text=True)
            text = run.stderr
    else:
        run = subprocess.run(["sudo", "-n", HELPER, str(pid), str(secs)],
                             capture_output=True, text=True)
        text = run.stdout
    got = pr.parse_perfstat(text) if run.returncode == 0 else None
    if got is None:
        raise Broken(f"hardware counters unreadable (exit {run.returncode}): "
                     f"{(run.stderr or run.stdout).strip()[-300:]}")
    return got


def window(srv_pid, srv_cpus, port, secs, gens, perf=True):
    """One window of a loaded server. gens = [(pid, threads)]."""
    try:
        p0, s0, g0 = box_stat(), proc_ticks(srv_pid), [proc_ticks(p) for p, _ in gens]
    except FileNotFoundError as e:
        raise Broken(f"a process of this window is gone before it started ({e.filename})")
    c0, t0 = info_field(port, "total_commands_processed"), time.monotonic_ns()
    got = counters(srv_pid, secs) if perf else time.sleep(secs)
    c1, t1 = info_field(port, "total_commands_processed"), time.monotonic_ns()
    try:
        p1, s1, g1 = box_stat(), proc_ticks(srv_pid), [proc_ticks(p) for p, _ in gens]
    except FileNotFoundError as e:
        raise Broken(f"a process exited inside the window ({e.filename}): "
                     "a generator ran out of requests or the server died")
    if c0 is None or c1 is None or int(c1) <= int(c0):
        raise Broken(f"INFO total_commands_processed unreadable or flat on port {port}")
    return {"srv_cpus": srv_cpus, "secs": secs, "cpu": pr.proc_stat_delta(p0, p1),
            "srv_ticks": s1 - s0, "gens": [[b - a, g[1]] for a, b, g in zip(g0, g1, gens)],
            "cmds": int(c1) - int(c0), "wall_ns": t1 - t0, "perf": got}


def idle_fraction(secs=1.0):
    a = box_stat()
    time.sleep(secs)
    d = pr.proc_stat_delta(a, box_stat())
    busy, total = sum(v[0] for v in d.values()), sum(v[1] for v in d.values())
    return 1.0 - (busy / total if total else 0.0)


def process_ticks():
    """{pid: (comm, ticks)} of every process on the box."""
    out = {}
    for d in pathlib.Path("/proc").iterdir():
        if not d.name.isdigit():
            continue
        try:
            raw = (d / "stat").read_text()
        except OSError:
            continue
        comm = raw[raw.index("(") + 1:raw.rindex(")")]
        f = raw.rsplit(")", 1)[1].split()
        out[int(d.name)] = (comm, int(f[11]) + int(f[12]))
    return out


def foreign_processes(before, after, top=5):
    """The processes other than this one that used CPU between two snapshots,
    as (comm, pid, cpu seconds). This run's own servers and generators have
    exited by then, so what is left is the rest of the box."""
    hz = os.sysconf("SC_CLK_TCK")
    me = os.getpid()
    used = [(comm, pid, (t - before.get(pid, (None, 0))[1]) / hz)
            for pid, (comm, t) in after.items() if pid != me]
    used = [u for u in used if u[2] > 0]
    return sorted(used, key=lambda u: -u[2])[:top]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["window"])
    ap.add_argument("--srv-pid", type=int, required=True)
    ap.add_argument("--srv-cpus", required=True)
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--secs", type=int, default=3)
    ap.add_argument("--gens", default="")
    for k in ("angle", "side"):
        ap.add_argument("--" + k, default="")
    for k in ("obs", "win"):
        ap.add_argument("--" + k, type=int, default=0)
    a = ap.parse_args()
    gens = [tuple(int(x) for x in g.split(":")) for g in a.gens.split(",") if g]
    try:
        w = window(a.srv_pid, a.srv_cpus, a.port, a.secs, gens)
    except Broken as e:
        print(f"perfgate_measure: {e}", file=sys.stderr)
        sys.exit(2)
    w.update(angle=a.angle, side=a.side, obs=a.obs, win=a.win)
    print(json.dumps(w, sort_keys=True))


if __name__ == "__main__":
    main()
