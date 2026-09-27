#!/usr/bin/env python3
"""web/src/commands.json is derived from the engine, not maintained by hand.

The site's command reference carried every field COMMAND DOCS already answers
— name, group, arity, flags, since, syntax, summary, complexity, compat — and
was edited by hand anyway. So it fell behind the moment a verb was added:
HRANDFIELD landed and the page that the sentence "all N commands carry their
real deviation" links to still listed the old set.

Asks a running kevy rather than parsing VERB_META. A regex over the source
undercounted by one on the first attempt, which is the failure this repository
keeps naming: the pattern rots and the count looks plausible.

Run: python3 tools/export_site_commands.py [--check]
"""

import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "web" / "src" / "commands.json"


def free_port() -> int:
    """A port nothing listens on now. A fixed one let a stray listener
    answer in the engine's place, and the check read that as a hang."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def read(f):
    line = f.readline()
    if not line:
        raise EOFError("connection closed mid-reply")
    t, rest = line[:1], line[1:].strip()
    if t in b"+-":
        return rest.decode()
    if t == b":":
        return int(rest)
    if t == b"$":
        n = int(rest)
        if n < 0:
            return None
        v = f.read(n)
        f.read(2)
        return v.decode("utf-8", "replace")
    if t == b"*":
        n = int(rest)
        return None if n < 0 else [read(f) for _ in range(n)]
    raise ValueError(f"unexpected reply type {t!r}")


def ask(sock, f, argv):
    sock.sendall(b"*%d\r\n" % len(argv) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in argv))
    return read(f)


def engine_tail(log) -> str:
    log.seek(0)
    lines = log.read().decode(errors="replace").strip().splitlines()[-5:]
    return "".join(f"\n  engine: {line}" for line in lines)


def harvest() -> list:
    binary = ROOT / "target" / "debug" / "kevy"
    if not binary.exists():
        sys.exit(f"export_site_commands: no engine at {binary} — cargo build -p kevy")
    # The engine's stderr goes to a file, not a pipe (a pipe nobody reads
    # fills and stalls the server) and not DEVNULL: when it never answers,
    # why it stopped is the only useful thing to print.
    log = tempfile.TemporaryFile()
    port = free_port()
    # Its own directory: an engine started in the repo root opens its store
    # there, and one that misreads its flags writes it (rootgate).
    home = tempfile.TemporaryDirectory(prefix="kevy-site-commands-")
    proc = subprocess.Popen([str(binary), "--port", str(port), "--no-aof"], cwd=home.name,
                            env={**dict(__import__("os").environ), "KEVY_BIND": "127.0.0.1"},
                            stdout=subprocess.DEVNULL, stderr=log)
    try:
        for _ in range(60):
            try:
                sock = socket.create_connection(("127.0.0.1", port), 1)
                break
            except OSError:
                if proc.poll() is not None:
                    break
                time.sleep(0.25)
        else:
            sys.exit("export_site_commands: the engine never accepted a connection "
                     f"(still running after 15s on port {port}){engine_tail(log)}")
        if proc.poll() is not None:
            sys.exit(f"export_site_commands: the engine exited with {proc.returncode} "
                     f"before accepting a connection on port {port}{engine_tail(log)}")
        f = sock.makefile("rb")
        # COMMAND LIST answers in VERB_META's declaration order — grouped by
        # family, which is how the reference page reads. Sorting would have
        # been a 205-line diff saying nothing.
        names = ask(sock, f, [b"COMMAND", b"LIST"])
        rows = []
        for name in names:
            doc = ask(sock, f, [b"COMMAND", b"DOCS", name.encode()])
            fields = dict(zip(doc[1][0::2], doc[1][1::2]))
            arity = ask(sock, f, [b"COMMAND", b"INFO", name.encode()])[0][1]
            rows.append({"name": name, "group": fields.get("group", ""), "arity": arity,
                         "flags": fields.get("flags", []), "since": fields.get("since", ""),
                         "syntax": fields.get("syntax", ""), "summary": fields.get("summary", ""),
                         "complexity": fields.get("complexity", ""),
                         "compat": fields.get("compat", "")})
        sock.close()
        return rows
    finally:
        proc.terminate()
        proc.wait(timeout=10)
        home.cleanup()


def main() -> int:
    rows = harvest()
    if len(rows) < 100:
        sys.exit(f"export_site_commands: the engine answered {len(rows)} commands — "
                 "that is a broken harvest, not a shrunken surface")
    # The exact shape the file already had — including the header it carried
    # while no generator existed to write it. `generated_from` and `count`
    # were a claim; this makes them true. One command per line so a real
    # change shows as one line, not as a re-indent.
    body = ",\n    ".join(json.dumps(r, separators=(", ", ": "), ensure_ascii=False) for r in rows)
    payload = ('{\n  "generated_from": "crates/kevy/src/verb_meta",\n'
               f'  "count": {len(rows)},\n  "commands": [\n    {body}\n  ]\n}}\n')

    if "--check" in sys.argv:
        if not OUT.exists() or OUT.read_text() != payload:
            print(f"export_site_commands: STALE — {OUT.relative_to(ROOT)} does not match the "
                  f"engine's {len(rows)} commands. Regenerate with "
                  "python3 tools/export_site_commands.py", file=sys.stderr)
            return 1
        print(f"export_site_commands: ok — {len(rows)} commands, derived from the engine")
        return 0
    OUT.write_text(payload)
    print(f"export_site_commands: wrote {len(rows)} commands to {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
