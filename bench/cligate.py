#!/usr/bin/env python3
"""cligate — kevy-cli against the real redis-cli, byte for byte.

The owner's bar (2026-09-17) is that kevy-cli is >= redis-cli. A reading of
redis-cli's source says what it does; this gate asks the binary. For every
case it runs the pinned redis-cli and kevy-cli with the same argv, stdin and
environment against the same Redis server, from the same fresh keyspace, and
compares stdout, stderr and exit code. Nothing on the redis-cli side is
written down by hand, so the expectation cannot drift from the thing it
describes.

A difference fails unless the case names a deviation from deviations.txt —
a place where redis-cli is wrong and kevy-cli is deliberately better. Such a
case must then state kevy-cli's own expected output, or the better behaviour
would be untested.

Floors, so the gate cannot pass by running nothing: every id in scope.txt
must be claimed by at least one case, every deviation must be used by a case
and every case's deviation must exist, and a run that selects no case is a
refusal, not a pass.

    python3 bench/cligate.py                 # builds kevy-cli for Linux first
    python3 bench/cligate.py --only RC-048

Both CLIs run inside the same container, so HOME, TERM, unix socket paths and
the libc they see are the same; kevy-cli is built in rust:<toolchain>-bookworm,
the Debian release the redis image is built on. Needs docker with host
networking.

Exit 0 all cases agree · 1 a case differs · 2 the gate could not run
"""

import argparse
import json
import os
import pathlib
import shlex
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
HERE = ROOT / "bench" / "cligate"
PORT = 16390
AUTH_PORT = 16391  # requirepass secret, plus ACL user alice / wonderland
SOCKET = "/cligate/redis.sock"
TIMEOUT_S = 10


def redis_image() -> str:
    anchors = json.loads((ROOT / "bench/COMPETITOR-ANCHORS.json").read_text())
    return f"redis:{anchors['anchors']['redis']['pinned']}"


def unescape(text: str) -> bytes:
    """C-style escapes in a case value: \\n \\r \\t \\\\ \\xHH."""
    out, i = bytearray(), 0
    raw = text.encode()
    while i < len(raw):
        c = raw[i]
        if c != 0x5C or i + 1 == len(raw):
            out.append(c)
            i += 1
            continue
        n = chr(raw[i + 1])
        if n == "x":
            out.append(int(raw[i + 2:i + 4], 16))
            i += 4
            continue
        out.append({"n": 10, "r": 13, "t": 9, "\\": 92, "0": 0}[n])
        i += 2
    return bytes(out)


def parse_cases(path: pathlib.Path):
    cases, cur = [], None
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        if line.startswith("["):
            head = line.strip()[1:-1]
            ids, _, name = head.partition(" ")
            cur = {"ids": ids.split(","), "name": name, "line": lineno,
                   "setup": [], "env": {}, "stdin": b"", "run": None}
            cases.append(cur)
            continue
        key, _, value = line.partition(":")
        key, value = key.strip(), value.strip()
        if cur is None:
            sys.exit(f"cligate: {path}:{lineno}: field before any case")
        if key == "setup":
            cur["setup"].append(value)
        elif key == "env":
            k, _, v = value.partition("=")
            cur["env"][k] = v
        elif key == "stdin":
            cur["stdin"] += unescape(expand(value))
        elif key in ("run", "deviation", "kevy.stdout", "kevy.stderr", "kevy.exit",
                     "kevy.stdout-contains"):
            cur[key] = value
        else:
            sys.exit(f"cligate: {path}:{lineno}: unknown field {key!r}")
    for c in cases:
        if c["run"] is None:
            sys.exit(f"cligate: case at line {c['line']} has no run:")
    return cases


def expand(text: str) -> str:
    return (text.replace("$AUTH_PORT", str(AUTH_PORT)).replace("$PORT", str(PORT))
            .replace("$SOCKET", SOCKET))


def argv_of(case) -> list:
    return shlex.split(expand(case["run"]))


def sh(args, **kw):
    return subprocess.run(args, capture_output=True, **kw)


class Reference:
    """The pinned redis-servers and a container holding the pinned redis-cli.

    Two servers: an open one every case starts from, and one with a password
    and an ACL user for the authentication flags. They share a volume with
    the CLI container so `-s` reaches a real unix socket. Containers are not
    --rm'd: a case may SHUTDOWN the server, and it is started again before
    the next run.
    """

    def __init__(self, image: str, binary: str):
        self.image = image
        self.binary = binary
        tag = os.getpid()
        self.volume = f"cligate-sock-{tag}"
        self.server = f"cligate-server-{tag}"
        self.auth = f"cligate-auth-{tag}"
        self.cli = f"cligate-cli-{tag}"

    def _start(self, name, *cmd):
        r = sh(["docker", "run", "-d", "--name", name, "--network", "host",
                "-v", f"{self.volume}:/cligate", self.image, *cmd])
        if r.returncode != 0:
            self.__exit__()
            sys.exit(f"cligate: could not start {name}: {r.stderr.decode()}")

    def __enter__(self):
        # The image's entrypoint drops to the redis user, which cannot bind a
        # socket in a fresh root-owned volume.
        sh(["docker", "run", "--rm", "-v", f"{self.volume}:/cligate", "--entrypoint", "chmod",
            self.image, "777", "/cligate"])
        self._start(self.server, "redis-server", "--port", str(PORT), "--save", "",
                    "--appendonly", "no", "--unixsocket", SOCKET, "--unixsocketperm", "777",
                    "--enable-debug-command", "yes")
        self._start(self.auth, "redis-server", "--port", str(AUTH_PORT), "--save", "",
                    "--appendonly", "no", "--requirepass", "secret",
                    "--user", "alice", "on", ">wonderland", "~*", "&*", "+@all")
        self._start(self.cli, "sleep", "infinity")
        r = sh(["docker", "cp", self.binary, f"{self.cli}:/usr/local/bin/kevy-cli"])
        if r.returncode != 0:
            self.__exit__()
            sys.exit(f"cligate: could not copy kevy-cli in: {r.stderr.decode()}")
        if not self.ensure_up():
            self.__exit__()
            sys.exit("cligate: reference server never answered PING")
        return self

    def __exit__(self, *exc):
        sh(["docker", "rm", "-f", self.server, self.auth, self.cli])
        sh(["docker", "volume", "rm", "-f", self.volume])

    def ensure_up(self) -> bool:
        for attempt in range(50):
            if self.run(["-p", str(PORT), "PING"], b"", {}).stdout == b"PONG\n":
                return True
            if attempt == 0:
                sh(["docker", "start", self.server])
            time.sleep(0.1)
        return False

    def run(self, argv, stdin, env, program="redis-cli"):
        envs = [a for k, v in env.items() for a in ("-e", f"{k}={v}")]
        try:
            return sh(["docker", "exec", "-i", *envs, self.cli, program, *argv],
                      input=stdin, timeout=TIMEOUT_S)
        except subprocess.TimeoutExpired:
            return subprocess.CompletedProcess(argv, 124, b"", b"<timed out>")

    def reset(self, setup):
        if not self.ensure_up():
            sys.exit("cligate: the reference server did not come back")
        self.run(["-p", str(PORT), "FLUSHALL"], b"", {})
        for line in setup:
            r = self.run(["-p", str(PORT), *shlex.split(expand(line))], b"", {})
            if r.returncode != 0:
                sys.exit(f"cligate: setup failed: {line}: {r.stderr.decode()}")


def own_name(text: bytes) -> bytes:
    """DEV-010: where redis-cli names itself, kevy-cli names itself."""
    return text.replace(b"redis-cli", b"kevy-cli")


def expected_kevy(case, ref):
    """What kevy-cli must print: redis-cli's bytes, or the deviation's."""
    if "deviation" not in case:
        return own_name(ref.stdout), own_name(ref.stderr), ref.returncode
    return (unescape(case.get("kevy.stdout", "")),
            unescape(case.get("kevy.stderr", "")),
            int(case.get("kevy.exit", "0")))


def agrees(case, want, got) -> bool:
    """A deviation may pin only a fragment of stdout (help text, version)."""
    if "kevy.stdout-contains" in case:
        return (unescape(case["kevy.stdout-contains"]) in got[0]
                and want[1:] == got[1:])
    return want == got


def show(label, want, got):
    return f"    {label}: redis-cli/expected {want!r}\n    {label}: kevy-cli          {got!r}"


def check_floors(cases, only):
    scope = [l.split("#")[0].strip() for l in (HERE / "scope.txt").read_text().splitlines()]
    scope = [s for s in scope if s]
    devs = {}
    for line in (HERE / "deviations.txt").read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            devs[line.split()[0]] = line
    claimed = {i for c in cases for i in c["ids"]}
    problems = [f"scope id {s} is claimed by no case" for s in scope if s not in claimed]
    used = {c.get("deviation") for c in cases} | {"DEV-010"}
    problems += [f"deviation {d} is used by no case" for d in devs if d not in used]
    problems += [f"case line {c['line']} names unknown deviation {c['deviation']}"
                 for c in cases if c.get("deviation") and c["deviation"] not in devs]
    if only:
        return []
    return problems


def build_linux_cli() -> str:
    """kevy-cli for the container's Debian, cached in a docker volume."""
    toolchain = next(l.split('"')[1] for l in (ROOT / "Cargo.toml").read_text().splitlines()
                     if l.startswith("rust-version"))
    out = ROOT / "target" / "cligate"
    out.mkdir(parents=True, exist_ok=True)
    r = subprocess.run(
        ["docker", "run", "--rm", "-v", f"{ROOT}:/src", "-v", "kevy-cligate-target:/target",
         "-v", f"{out}:/out", "-w", "/src", "-e", "CARGO_TARGET_DIR=/target",
         f"rust:{toolchain}-bookworm", "sh", "-c",
         "cargo build --release --locked -p kevy-cli && cp /target/release/kevy-cli /out/"],
        capture_output=True)
    if r.returncode != 0:
        sys.stderr.write(r.stderr.decode()[-4000:])
        sys.exit("cligate: building kevy-cli for Linux failed")
    return str(out / "kevy-cli")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--kevy-cli", help="a Linux kevy-cli to use instead of building one")
    ap.add_argument("--only", help="run only cases claiming this id")
    ap.add_argument("--determinism", action="store_true",
                    help="run redis-cli twice per case and report cases whose output "
                         "differs between runs — such a case cannot be compared")
    ap.add_argument("--show-reference", action="store_true",
                    help="print what redis-cli produced for each selected case and stop")
    args = ap.parse_args()
    binary = args.kevy_cli or build_linux_cli()
    cases = parse_cases(HERE / "cases.txt")
    problems = check_floors(cases, args.only)
    selected = [c for c in cases if not args.only or args.only in c["ids"]]
    if not selected:
        print("cligate: no case selected — refusing to report a pass over nothing")
        return 2

    failed = 0
    with Reference(redis_image(), binary) as ref:
        for case in selected:
            argv = argv_of(case)
            ref.reset(case["setup"])
            r = ref.run(argv, case["stdin"], case["env"])
            ref.reset(case["setup"])
            if args.show_reference:
                print(f"[{','.join(case['ids'])} {case['name']}] exit={r.returncode}")
                print(f"    stdout {r.stdout!r}\n    stderr {r.stderr!r}")
                continue
            if args.determinism:
                ref.reset(case["setup"])
                again = ref.run(argv, case["stdin"], case["env"])
                if (r.stdout, r.stderr, r.returncode) != (again.stdout, again.stderr,
                                                          again.returncode):
                    failed += 1
                    print(f"UNSTABLE [{','.join(case['ids'])} {case['name']}] "
                          f"(cases.txt:{case['line']}): {r.stdout[:120]!r} vs {again.stdout[:120]!r}")
                continue
            k = ref.run(argv, case["stdin"], case["env"], program="kevy-cli")
            want = expected_kevy(case, r)
            got = (k.stdout, k.stderr, k.returncode)
            if agrees(case, want, got):
                continue
            failed += 1
            print(f"FAIL [{','.join(case['ids'])} {case['name']}] (cases.txt:{case['line']})")
            print(f"    argv: {argv}")
            for label, w, g in zip(("stdout", "stderr", "exit"), want, got):
                if w != g:
                    print(show(label, w, g))

    for p in problems:
        print(f"FLOOR {p}")
    ran = len(selected)
    print(f"cligate: {ran - failed}/{ran} cases agree"
          + (f", {len(problems)} floor problem(s)" if problems else ""))
    return 1 if failed or problems else 0


if __name__ == "__main__":
    sys.exit(main())
