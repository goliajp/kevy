#!/usr/bin/env python3
"""perfgate: two kevy builds on one box, in one run. See bench/perfgate.sh."""

import argparse
import json
import os
import pathlib
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "tools"))
import perfgate_angles as ang  # noqa: E402
import perfgate_build as pb  # noqa: E402
import perfgate_measure as pm  # noqa: E402
import perfgate_table as pt  # noqa: E402

CONFIG = json.loads((HERE / "perfgate.json").read_text())
PORT = int(os.environ.get("PERFGATE_PORT", "7001"))


def die(msg, code=2):
    print(f"perfgate: {msg}", file=sys.stderr)
    sys.exit(code)


def topology():
    name = os.environ.get("PERFGATE_BOX") or socket.gethostname().split(".")[0]
    boxes = CONFIG["boxes"]
    if name not in boxes:
        die(f"no topology for box '{name}' in bench/perfgate.json "
            f"(known: {', '.join(boxes)}); add one or set PERFGATE_BOX")
    t = dict(boxes[name], box=name)
    t["srv_list"] = pm.pr.cpu_list(t["srv_cpus"])
    t["cli_list"] = pm.pr.cpu_list(t["cli_cpus"])
    return t


def side_env(var):
    env = dict(os.environ, KEVY_BIND="127.0.0.1", KEVY_IO_URING="1")
    for kv in shlex.split(os.environ.get(var, "")):
        k, _, v = kv.partition("=")
        env[k] = v
    return env


class Server:
    """One kevy process, pinned to the server cpus, on a fresh data dir."""

    def __init__(self, binary, env, topo, cluster, rundir):
        data = pathlib.Path(tempfile.mkdtemp(dir=rundir))
        argv = [binary, "--threads", str(topo["srv_threads"]), "--port", str(PORT)]
        argv += (["--cluster"] if cluster else []) + ["--no-aof", "--dir", str(data)]
        self.log = open(data / "server.log", "wb")
        cpus = topo["srv_list"]
        self.proc = subprocess.Popen(argv, env=env, stdout=self.log, stderr=subprocess.STDOUT,
                                     preexec_fn=lambda: os.sched_setaffinity(0, cpus))
        ports = [PORT] + ([PORT + 1 + i for i in range(topo["srv_threads"])] if cluster else [])
        self.ports = ports
        deadline = time.time() + 20
        while not all(pm.ping(p) for p in ports):
            if self.proc.poll() is not None or time.time() > deadline:
                self.stop()
                tail = (data / "server.log").read_text(errors="replace").splitlines()[-5:]
                die(f"server did not come up: {' '.join(argv)}\n" + "\n".join(tail))
            time.sleep(0.1)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()
        # an io_uring server's listeners outlive its exit by a few ms, and the
        # next side binds the same ports at once
        deadline = time.time() + 2
        while any(listening(p) for p in self.ports) and time.time() < deadline:
            time.sleep(0.01)


def listening(port):
    try:
        socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        return True
    except OSError:
        return False


def stop_all(procs):
    for p in procs:
        if p.poll() is None:
            p.kill()
    for p in procs:
        p.wait()


def observe(angle, binary, env, topo, rundir, windows, secs):
    """One side of one angle: fresh server, warm, load, windows."""
    topo = ang.topology(angle, topo)
    srv = Server(binary, env, topo, ang.cluster(angle), rundir)
    gens = []
    try:
        ang.warm(angle, PORT, topo["srv_threads"])
        if angle == "hybrid_p95":
            out = subprocess.run(ang.latency_cmd(PORT), capture_output=True, text=True,
                                 preexec_fn=lambda: os.sched_setaffinity(0, topo["cli_list"][:1]))
            if out.returncode != 0 or not out.stdout.strip().isdigit():
                die(f"{angle}: latency client failed: {out.stderr.strip()[-200:]}")
            return {"p95_us": float(out.stdout.strip()), "rss": pm.rss_mb(srv.proc.pid)}
        for argv, threads in ang.generators(angle, PORT, topo["srv_threads"], topo["cli_threads"]):
            gens.append((subprocess.Popen(argv, stdout=subprocess.DEVNULL,
                                          stderr=subprocess.DEVNULL), threads))
        time.sleep(CONFIG["ramp_secs"])
        ws = [pm.window(srv.proc.pid, topo["srv_cpus"], PORT, secs,
                        [(p.pid, t) for p, t in gens]) for _ in range(windows)]
        obs = pt.pr.observation(ws)
        obs["rss"] = pm.rss_mb(srv.proc.pid)
        return obs
    except pm.Broken as e:
        die(f"{angle}: {e}")
    finally:
        stop_all([p for p, _ in gens])
        srv.stop()


def run_rounds(sides, angles, rounds, topo, windows, secs):
    """ABBA across rounds and angles: the side that goes first alternates,
    so drift over the run lands on both sides equally."""
    results = {a: [] for a in angles}
    with tempfile.TemporaryDirectory(prefix="perfgate-") as rundir:
        for r in range(rounds):
            for i, angle in enumerate(angles):
                order = (0, 1) if (r + i) % 2 == 0 else (1, 0)
                obs = [None, None]
                for s in order:
                    obs[s] = observe(angle, sides[s]["bin"], sides[s]["env"], topo, rundir,
                                     windows, secs)
                results[angle].append(tuple(obs))
                print(f"perfgate: round {r + 1}/{rounds} {angle} done", file=sys.stderr, flush=True)
    return results


def parse(argv):
    ap = argparse.ArgumentParser(prog="perfgate.sh")
    sub = ap.add_subparsers(dest="mode", required=True)
    for name in ("compare", "gate", "callgrind", "prepare"):
        p = sub.add_parser(name)
        p.add_argument("sides", nargs="*")
        p.add_argument("--rounds", type=int, default=CONFIG["rounds"])
        p.add_argument("--angles", default="")
        p.add_argument("--ref", default=CONFIG["reference"])
        p.add_argument("--windows", type=int, default=CONFIG["windows"])
        p.add_argument("--secs", type=int, default=CONFIG["window_secs"])
        p.add_argument("--ops", type=int, default=200_000)
        p.add_argument("--for", dest="for_mode", default="compare")
    return ap.parse_args(argv)


def side_specs(a):
    mode = a.for_mode if a.mode == "prepare" else a.mode
    if mode == "gate":
        return [a.ref, a.sides[0] if a.sides else "HEAD"]
    if len(a.sides) != 2:
        die(f"{mode} needs two builds: A B (a path, or a git rev like v6.4.0 or HEAD+kevy-alloc)")
    return a.sides


def resolve(specs, build_missing):
    out = []
    for spec, var in zip(specs, ("A_ENV", "B_ENV")):
        try:
            path, label = pb.binary(spec, build_missing)
        except subprocess.CalledProcessError as e:
            die(f"{spec}: neither a file nor a buildable git rev ({(e.stderr or '').strip()[-200:]})")
        ver = subprocess.run([path, "--version"], capture_output=True, text=True).stdout.strip()
        out.append({"spec": spec, "bin": path, "label": label, "version": ver.splitlines()[0]
                    if ver else "?", "env": side_env(var), "env_note": os.environ.get(var, "")})
    return out


# Batch jobs on the bench boxes step aside once a benchmark holds the lock:
# a watcher sees it within 15 s and stops them. The box gets this long to
# go quiet before the run is refused.
QUIET_WAIT_SECS = 60


def preflight(topo):
    need = float(os.environ.get("PERFGATE_IDLE_MIN", CONFIG["idle_min"]))
    deadline = time.time() + QUIET_WAIT_SECS
    idle = pm.idle_fraction()
    while idle < need and time.time() < deadline:
        idle = pm.idle_fraction()
    print(f"# box {topo['box']}: idle {idle:.1%} before start, load average "
          f"{os.getloadavg()[0]:.2f}", flush=True)
    if idle < need:
        before = pm.process_ticks()
        time.sleep(1)
        busy = pm.foreign_processes(before, pm.process_ticks())
        die(f"box busy (idle {idle:.1%} < {need:.0%}); run again when it is "
            "quiet. Using the CPU now: " + ", ".join(f"{c} ({p}) {s:.2f}s" for c, p, s in busy))


def main():
    # a TERM from a timeout unwinds like an error, so every server and
    # generator this run started is stopped on the way out
    signal.signal(signal.SIGTERM, lambda *_: die("terminated"))
    a = parse(sys.argv[1:])
    specs = side_specs(a)
    if a.mode == "prepare":
        resolve(specs, True)
        return 0
    sides = resolve(specs, False)
    if a.mode == "callgrind":
        import perfgate_callgrind as cg
        return cg.run(sides, a.angles.split() or ["get", "set"], a.ops, PORT)
    angles = a.angles.split() or CONFIG["angles"]
    unknown = [x for x in angles if x not in ang.ANGLES]
    if unknown:
        die(f"unknown angle(s) {' '.join(unknown)}; known: {' '.join(ang.ANGLES)}")
    topo = topology()
    os.sched_setaffinity(0, topo["cli_list"])
    pt.header(a.mode, sides, topo, a.rounds, a.windows, a.secs)
    preflight(topo)
    before, t0 = pm.process_ticks(), time.time()
    results = run_rounds(sides, angles, a.rounds, topo, a.windows, a.secs)
    code = pt.report(a.mode, results, CONFIG)
    print(f"# {time.time() - t0:.0f} s; other processes that used CPU during the run: " +
          (", ".join(f"{c} ({p}) {s:.1f}s" for c, p, s in
                     pm.foreign_processes(before, pm.process_ticks())) or "none"))
    return code


if __name__ == "__main__":
    sys.exit(main())
