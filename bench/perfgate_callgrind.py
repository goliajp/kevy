"""perfgate callgrind: exact instructions per op, per function, A against B.

Each side runs one shard under `valgrind --tool=callgrind` with
instrumentation off; the angle's keys are written first, then
instrumentation is switched on for exactly OPS requests from
redis-benchmark and the counts are dumped when they are done. The
counts do not depend on the box's load, its clock or its neighbours, so the
same build gives the same numbers on any quiet or busy machine. It answers
"did the instructions per op change, and where"; it says nothing about
cycles, cache misses or the kernel.

Valgrind has no io_uring support, so the server runs its epoll reactor
(KEVY_IO_URING=0): the command path is the same code, the reactor is not.
"""

import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time

import perfgate_angles as ang
import perfgate_measure as pm

TOP = 25
# well past a shard's tick (100 ms), which runs slower under valgrind
TICK_WAIT = 1.0
LINE = re.compile(r"^\s*([\d,]+)\s+(?:\([\s\d.]+%\)\s+)?(\S.*)$")
HASH = re.compile(r"::h[0-9a-f]{16}\b")


def function_name(field):
    """'file:function [obj]' -> 'function', without the rust symbol hash."""
    field = re.sub(r"\s+\[[^\]]*\]$", "", field)
    m = re.match(r"^((?:[^:]|::)*?):(?!:)(.*)$", field)
    name = m.group(2) if m and m.group(2) else field
    return HASH.sub("", name).strip()


def annotate(path):
    """{function: Ir} of one callgrind output, exclusive counts."""
    out = subprocess.run(["callgrind_annotate", "--inclusive=no", "--threshold=100", "--auto=no",
                          str(path)],
                         capture_output=True, text=True, check=True).stdout
    total, funcs = None, {}
    for line in out.splitlines():
        m = LINE.match(line)
        if not m:
            continue
        ir, rest = int(m.group(1).replace(",", "")), m.group(2)
        if "PROGRAM TOTALS" in rest:
            total = ir
            continue
        name = function_name(rest)
        funcs[name] = funcs.get(name, 0) + ir
    return total, funcs


def control(pid, *args):
    subprocess.run(["callgrind_control", *args, str(pid)], capture_output=True, check=True)


def wait_up(proc, port, secs=120):
    deadline = time.time() + secs
    while not pm.ping(port):
        if proc.poll() is not None or time.time() > deadline:
            proc.kill()
            sys.exit(f"perfgate callgrind: server under valgrind did not come up on {port}")
        time.sleep(0.5)


def one(side, angle, ops, port, work):
    """Run one side under callgrind; returns (ops, counted, total Ir, {function: Ir}).

    ops is the request count sent; counted is the server's own command
    counter across the same span, printed as a check on it."""
    warm_cmd, load_cmd = ang.CALLGRIND[angle]
    out = work / f"cg.{angle}.{side['name']}"
    env = dict(side["env"], KEVY_IO_URING="0")
    argv = [side["bin"], "--port", str(port), "--no-aof", "--dir", str(work / "data")]
    if angle.startswith("x"):
        # Two shards spin while idle, and valgrind runs one thread at a time,
        # so how long a shard spins before the other gets the core depends
        # on timing: the same binary counted ±18% between runs. Parked
        # shards count only the work and the wake-ups it causes.
        conf = work / "parked.toml"
        conf.write_text("[advanced]\nspin_limit = 0\n")
        argv += ["--threads", "2", "--config", str(conf)]
    else:
        argv += ["--threads", "1"]
    proc = subprocess.Popen(["valgrind", "--tool=callgrind", "--instr-atstart=no",
                             f"--callgrind-out-file={out}", *argv],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_up(proc, port)
        if warm_cmd in ang.ANGLES:
            ang.warm(warm_cmd, port, 1)
        elif warm_cmd:
            ang.run_quiet(ang.bench(port, *warm_cmd.split(), n=100_000, keyspace=100_000, pipe=16))
        # each shard publishes its command count on its tick, so the count
        # is read once every shard has ticked since the last command
        time.sleep(TICK_WAIT)
        c0 = int(pm.info_field(port, "total_commands_processed"))
        control(proc.pid, "-i", "on")
        ang.run_quiet(ang.bench(port, *load_cmd.split(), n=ops, keyspace=100_000, conns=4, pipe=16))
        control(proc.pid, "-d")
        time.sleep(TICK_WAIT)
        c1 = int(pm.info_field(port, "total_commands_processed"))
    finally:
        proc.terminate()
        proc.wait()
        shutil.rmtree(work / "data", ignore_errors=True)
    # the dump is <out>.1; <out> itself is written at exit and holds the rest
    total, funcs = annotate(pathlib.Path(f"{out}.1"))
    return ops, c1 - c0, total, funcs


def table(angle, a, b):
    (na, ca, ta, fa), (nb, cb, tb, fb) = a, b
    print(f"\n## {angle}: {na} requests a side (server counted A {ca}, B {cb}) — "
          "instructions per op\n")
    print(f"{'A':>10}{'B':>10}{'B-A':>9}  function")
    print(f"{ta / na:>10.1f}{tb / nb:>10.1f}{tb / nb - ta / na:>+9.1f}  TOTAL "
          f"(B/A {tb / nb / (ta / na):.4f})")
    names = sorted(set(fa) | set(fb), key=lambda f: -max(fa.get(f, 0) / na, fb.get(f, 0) / nb))
    for f in names[:TOP]:
        x, y = fa.get(f, 0) / na, fb.get(f, 0) / nb
        print(f"{x:>10.1f}{y:>10.1f}{y - x:>+9.1f}  {f[:110]}")
    moved = sorted(names, key=lambda f: -abs(fb.get(f, 0) / nb - fa.get(f, 0) / na))[:10]
    print("\nlargest changes:")
    for f in moved:
        x, y = fa.get(f, 0) / na, fb.get(f, 0) / nb
        print(f"{x:>10.1f}{y:>10.1f}{y - x:>+9.1f}  {f[:110]}")


def run(sides, angles, ops, port):
    for tool in ("valgrind", "callgrind_annotate", "callgrind_control", "redis-benchmark"):
        if not shutil.which(tool):
            sys.exit(f"perfgate callgrind: {tool} not found")
    bad = [a for a in angles if a not in ang.CALLGRIND]
    if bad:
        sys.exit(f"perfgate callgrind: no single-shard shape for {' '.join(bad)}; "
                 f"known: {' '.join(ang.CALLGRIND)}")
    for name, s in zip("AB", sides):
        s["name"] = name
        print(f"# {name}: {s['label']} — {s['version']} — {s['bin']}")
    print(f"# callgrind, one shard (two for the x angles), epoll reactor, {ops} requests per angle "
          "(-c 4 -P 16), counted from the first request to the last")
    with tempfile.TemporaryDirectory(prefix="perfgate-cg-") as d:
        for angle in angles:
            n = min(ops, ang.CALLGRIND_OPS.get(angle, ops))
            got = [one(s, angle, n, port, pathlib.Path(d)) for s in sides]
            table(angle, *got)
    return 0


if __name__ == "__main__":
    sys.exit("run through bench/perfgate.sh callgrind A B")
