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
        # A command wearing a CLI in front of it is still a command. The
        # landing page's install snippet read
        #   redis-cli IDX.CREATE users ON HASH PREFIX user: FIELDS city
        # which is not a syntax the engine accepts — and this gate skipped
        # the whole line as shell, because it does not begin with a verb.
        # It was the same wrong form a reader had already reported from
        # the terminal, sitting two screens above it, unexamined.
        line = re.sub(r"^(redis-cli|kevy-cli|valkey-cli)\b(\s+-[a-zA-Z]\s*\S+)*\s+", "", line)
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

    # The built site, which is what a visitor gets. Reading the sources
    # would test what the content says rather than what the page serves,
    # and those are two different files with a renderer between them.
    dist = ROOT / "web/dist"
    if not dist.exists():
        print("check_site_commands: no web/dist — run npm run build in web/")
        return 1

    blocks = []
    pages = sorted(dist.rglob("index.html"))
    # A gate that finds nothing must not pass. An empty dist is a broken
    # build, not a site with no examples in it.
    if len(pages) < 100:
        print(f"check_site_commands: only {len(pages)} pages in web/dist — expected the whole site")
        return 1
    for f in pages:
        if "/docs/" in str(f):
            continue  # generated from docs/, gated separately
        if "/changelog/" in str(f):
            # A changelog's code blocks are EVIDENCE, not instructions:
            # benchmark transcripts, the error message a fixed bug used
            # to print, output from a version that no longer exists. The
            # one that tripped this gate is a `=== PING:` / `PONG`
            # benchmark capture from v2.0.15 — running `PONG` as a
            # command against today's build tests nothing, and the
            # mismatch only grows as history accumulates.
            continue
        html = f.read_text(encoding="utf-8")
        for m in re.finditer(r"<pre><code[^>]*>(.*?)</code></pre>", html, re.S):
            body = (
                m.group(1)
                .replace("&gt;", ">").replace("&lt;", "<")
                .replace("&quot;", '"').replace("&#x27;", "'").replace("&amp;", "&")
            )
            blocks.append((f.relative_to(ROOT), body))

    # The playground's scenarios are commands on the site too, but they live
    # in JavaScript rather than in a <pre>, so this gate could not see them —
    # and that is where the one command a visitor reported as broken was.
    # web/verify.mjs runs them against the wasm engine in a browser, which is
    # the authority; this is the cheap fast copy that says which line and why
    # without a build and a Chromium.
    # The landing page is rendered in the browser, so its code blocks are
    # not in dist/index.html and this gate never saw them. That is where
    # the install snippet sat with `IDX.CREATE … ON HASH PREFIX … FIELDS
    # city` in it — the exact form a reader had already reported from the
    # terminal two screens below, on the same page, for as long as the
    # gate had been passing.
    app = ROOT / "web/src/App.tsx"
    if not app.exists():
        print("check_site_commands: web/src/App.tsx is missing — the landing page moved")
        return 1
    snippets = re.findall(r"^const [A-Z_]+ = `(.*?)`$", app.read_text(encoding="utf-8"), re.S | re.M)
    if len(snippets) < 2:
        print(f"check_site_commands: only {len(snippets)} landing-page snippets — the parse is wrong")
        return 1
    for body in snippets:
        blocks.append((app.relative_to(ROOT), body))

    scen = ROOT / "web/src/scenarios.ts"
    if not scen.exists():
        print("check_site_commands: web/src/scenarios.ts is missing — the playground moved")
        return 1
    lines = [
        c
        for arr in re.findall(r"lines:\s*\[(.*?)\n\s*\],", scen.read_text(encoding="utf-8"), re.S)
        for c in re.findall(r"^\s*'(.*?)',\s*$", arr, re.M)
    ]
    if len(lines) < 40:
        print(f"check_site_commands: only {len(lines)} playground commands — the parse is wrong")
        return 1
    blocks.append((scen.relative_to(ROOT), "\n".join(c.replace("\\'", "'") for c in lines)))

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
                    # `usage:` belongs here: a verb that exists but was
                    # called with a shape it does not accept answers with
                    # its usage string, and that is our mistake in exactly
                    # the way a wrong arity is. Without it this gate read
                    # `IDX.CREATE … ON HASH PREFIX … FIELDS city` — the
                    # very form a reader had reported — as a legitimate
                    # reply and passed.
                    if re.search(r"unknown command|wrong number of arguments|"
                                 r"syntax error|unknown subcommand|usage:", err, re.I):
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
    # A floor. This gate reported "ok: 0 commands" against a site full of
    # them, because the renderer emitted bare <pre> where it looks for
    # <pre><code> — a green line meaning it had checked nothing. Finding
    # nothing is a broken selector, not a site without examples.
    if n < 50:
        print(f"check_site_commands: only {n} commands found — the site has more than that.")
        print("  A selector that matches nothing reports success. Check what the pages emit.")
        return 1
    print(f"ok: {n} commands in the site's examples, all of them run")
    return 0


if __name__ == "__main__":
    sys.exit(main())
