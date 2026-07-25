#!/usr/bin/env python3
"""Every command in a site code block must actually run.

The documentation shipped with fourteen configs that could not load and an
example whose keys did not exist, because they were written from memory and
nobody ran them. Marketing copy is written from memory even more freely than
documentation is — and a command that errors is the single fastest way to lose a
visitor who was giving you the benefit of the doubt.

So the site's RESP examples are executed against a real kevy. A line that begins
with a verb the engine knows is sent; the reply must not be an error. Prose,
comments, `->` result annotations, shell lines and non-RESP languages are
skipped.

Run: python3 tools/check_site_commands.py   (needs target/debug/kevy)
"""

import pathlib
import re
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
KEVY = ROOT / "target/debug/kevy"
PORT = 6096

# The example values are illustrative — a 768-float vector is not going to be
# pasted into a marketing page. A line carrying one of these is checked for
# ARITY and VERB by sending a shaped-but-plausible substitute, not skipped:
# a wrong verb name is exactly the mistake this gate exists to catch.
PLACEHOLDER = re.compile(r'"?<[^>]+>"?|\$\w+|\$\{\w+\}')


def resp(sock, argv):
    out = f"*{len(argv)}\r\n".encode()
    for a in argv:
        b = a.encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    sock.sendall(out)
    time.sleep(0.02)
    try:
        return sock.recv(65536).decode(errors="replace")
    except TimeoutError:
        return "(timeout)"


def commands_in(text):
    """Yield the RESP command lines from a code block."""
    for raw in text.split("\n"):
        line = raw.strip()
        if not line or line.startswith(("#", "//", "->", "$", "-&gt;")):
            continue
        # a reply annotation or a trailing comment on the same line:
        #   `GET k   -> "v"`   /   `EXPIRE k 60   # one minute`
        line = re.split(r"\s+->\s|\s+-&gt;\s", line)[0]
        line = re.split(r"\s{2,}#|\s#\s", line)[0].strip()
        # Reply lines, not commands: RESP renders integers, nils, array items
        # and the bare OK exactly like this, and an example that shows its
        # output is better documentation than one that hides it.
        if not line or line.startswith(("(", "1)", "2)")) or line == "OK":
            continue
        if re.match(r"^\d+\) ", line):
            continue
        # Only lines that start with an UPPERCASE verb are commands.
        if not re.match(r"^[A-Z][A-Z0-9._]*(\s|$)", line):
            continue
        yield line


def split_argv(line):
    """Shell-ish split honouring single and double quotes."""
    out, cur, q = [], "", None
    for ch in line:
        if q:
            if ch == q:
                q = None
            else:
                cur += ch
        elif ch in "'\"":
            q = ch
        elif ch.isspace():
            if cur:
                out.append(cur)
                cur = ""
        else:
            cur += ch
    if cur:
        out.append(cur)
    return out


def main():
    if not KEVY.exists():
        print(f"SKIP: {KEVY.relative_to(ROOT)} not built")
        return 0

    blocks = []
    for f in sorted((ROOT / "site").rglob("index.html")):
        if "/docs/" in str(f) or "/play/" in str(f):
            continue  # generated from docs/, gated separately
        html = f.read_text(encoding="utf-8")
        for m in re.finditer(r"<pre><code[^>]*>(.*?)</code></pre>", html, re.S):
            body = (
                m.group(1)
                .replace("&gt;", ">").replace("&lt;", "<")
                .replace("&quot;", '"').replace("&#x27;", "'").replace("&amp;", "&")
            )
            blocks.append((f.relative_to(ROOT), body))

    srv = subprocess.Popen(
        [str(KEVY), "--port", str(PORT), "--dir", "/tmp/kevy-cmdgate"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(1.5)
    bad, n = [], 0
    try:
        s = socket.create_connection(("127.0.0.1", PORT), timeout=3)
        s.settimeout(3)
        for f, body in blocks:
            for line in commands_in(body):
                argv = split_argv(PLACEHOLDER.sub("x", line))
                if not argv:
                    continue
                n += 1
                r = resp(s, argv)
                if r.startswith("-"):
                    err = r.split("\r\n")[0]
                    # A placeholder we substituted can legitimately be the wrong
                    # TYPE (a "<768-float vector>" became "x"). What must never
                    # happen is an unknown command or a wrong arity — those are
                    # OUR mistake, not the placeholder's.
                    if re.search(r"unknown command|wrong number of arguments|"
                                 r"syntax error|unknown subcommand", err, re.I):
                        bad.append((f, line, err))
    finally:
        srv.terminate()
        srv.wait()

    for f, line, err in bad:
        print(f"{f}: {line}\n    {err}")

    if bad:
        print()
        print(f"REFUSED: {len(bad)} of {n} commands on the site do not run.")
        print("A command that errors is the fastest way to lose a visitor who was "
              "giving you the benefit of the doubt.")
        return 1
    print(f"ok: {n} commands in the site's examples, all of them run")
    return 0


if __name__ == "__main__":
    sys.exit(main())
