#!/usr/bin/env python3
"""Every ```toml block in the docs must be a config the server will actually load.

Three of them were not. `port = 6004` with no `[server]` section is a fatal
`unknown section []`, and `allow_dialects = ["5.1"]` was a parse error until the
parser learned about arrays. Both were copy-pasteable, both were in the docs for
months, and both hand a first-time reader a server that refuses to start — which
is the worst possible first impression, because it looks like the software is
broken rather than the instructions.

So it gets a gate rather than a proofread. Each block is written to a temp file
and fed to the real binary. A block that names a port has it rewritten to a free
one; a block that is a FRAGMENT (an excerpt showing one section, with a `…` or a
comment saying so) is skipped only if it is explicitly marked `toml,fragment`.

Run: python3 tools/check_doc_toml.py   (needs target/debug/kevy)
"""

import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
KEVY = ROOT / "target/debug/kevy"
PORT = 6097

FENCE = re.compile(r"```toml(?P<attrs>[^\n]*)\n(?P<body>.*?)```", re.S)


def loads(body):
    """Feed the block to the real binary. Returns None on success, else the
    first line the server complained with."""
    src = re.sub(r"(?m)^(\s*port\s*=\s*)\d+", rf"\g<1>{PORT}", body)
    src = re.sub(r"(?m)^(\s*\w*port\w*_base\s*=\s*)\d+", rf"\g<1>{PORT + 100}", src)
    with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as f:
        f.write(src)
        path = f.name
    try:
        # The server does not exit on success — it starts serving. Give it a
        # moment, then kill it: what we are testing is that it got past config.
        r = subprocess.run(
            ["sh", "-c", f"'{KEVY}' --config {path} 2>&1 & P=$!; sleep 1.2; kill $P 2>/dev/null"],
            capture_output=True,
            text=True,
            timeout=20,
        )
        out = (r.stdout or r.stderr).strip()
        first = out.splitlines()[0] if out else "(no output)"
        if "kevy-config:" in out or "error" in first.lower():
            return first
        return None
    finally:
        pathlib.Path(path).unlink(missing_ok=True)


def main():
    if not KEVY.exists():
        print(f"SKIP: {KEVY.relative_to(ROOT)} not built "
              "(cargo build -p kevy --bin kevy)")
        return 0

    bad = []
    n = 0
    for md in sorted(ROOT.glob("docs/**/*.md")):
        text = md.read_text(encoding="utf-8")
        for m in FENCE.finditer(text):
            if "fragment" in m.group("attrs"):
                continue
            body = m.group("body")
            # A block with no key at all is prose in a fence, not a config.
            if not re.search(r"(?m)^\s*\w+\s*=", body):
                continue
            # Not every ```toml in these docs is a kevy.toml. Cargo.toml appears
            # constantly — dependency lines, feature tiers, the [profile.iot]
            # block. Feeding those to the server proves nothing about either
            # file. A kevy config is one that names a kevy section, or that says
            # so in its first comment.
            if re.search(r"(?m)^\s*\[(package|dependencies|dev-dependencies|"
                         r"build-dependencies|profile|workspace|features|lib|bin)\b", body):
                continue
            if re.search(r"=\s*\{", body):  # a Cargo dependency table
                continue
            is_kevy = re.search(
                r"(?m)^\s*\[(server|lua|cluster|replication|persistence|"
                r"limits|index|feed|log)\b", body
            ) or re.search(r"(?im)^\s*#.*kevy[\w.-]*\.toml", body)
            if not is_kevy:
                continue
            n += 1
            line = text[: m.start()].count("\n") + 1
            err = loads(body)
            if err:
                bad.append((md.relative_to(ROOT), line, err))

    for f, line, err in bad:
        print(f"{f}:{line}: this config does not load — {err}")

    if bad:
        print()
        print(f"REFUSED: {len(bad)} of {n} TOML blocks in the docs do not load.")
        print("A config in the documentation that the server rejects is worse than "
              "no example: it reads as though the software is broken.")
        print("Mark a genuine excerpt as ```toml,fragment if it is not meant to "
              "stand alone.")
        return 1
    print(f"ok: {n} TOML blocks in the docs, all of them load")
    return 0


if __name__ == "__main__":
    sys.exit(main())
