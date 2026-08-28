#!/usr/bin/env python3
"""The TypeScript markdown renderer must agree with the Python one, exactly.

web/src/md.ts is a port of tools/md.py. It renders every documentation page
on the site, and 986 internal links depend on its anchor scheme alone — a
slug that differs by one character is 986 silent 404s that no build error
would report.

So the port is not asserted, it is measured: every markdown file in the
repository goes through both implementations and the first byte of
difference fails this gate. It runs in CI, which means the two cannot drift
after the day the port was written either.

The port exists because the site moved from a Python generator to the Vite
build the landing page uses; when the Python renderer is finally deleted,
this gate goes with it, and not before.

Run: python3 tools/check_md_port.py
Exit: 0 pass, 1 the two renderers disagree, 2 refused (nothing was compared).
"""

import json
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from md import render  # noqa: E402

# Everything a reader can reach, plus the ones only the gates read. Third-party
# and generated trees are somebody else's markdown.
SKIP = ("node_modules", "/target/", "/dist/", "/.git/", "web/node_modules")


def sources():
    # git ls-files rather than rglob: rglob walks target/ and node_modules
    # in full before any filter runs, which takes minutes. What git tracks
    # is also exactly the set that ships.
    out = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "*.md"],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    for rel in sorted(out):
        if any(t in "/" + rel for t in SKIP):
            continue
        yield ROOT / rel


HARNESS = r"""
import { readFileSync, writeFileSync } from 'node:fs'
import { render } from './src/md.ts'

const files = JSON.parse(readFileSync(process.argv[2], 'utf8'))
const out = {}
for (const f of files) {
  const { html, toc } = render(readFileSync(f, 'utf8'))
  out[f] = { html, toc }
}
writeFileSync(process.argv[3], JSON.stringify(out))
"""


def main():
    files = [str(p) for p in sources()]
    if not files:
        # A gate that finds nothing must not pass: an empty file list is a
        # broken glob, not a clean repository.
        sys.exit("check_md_port: found no markdown at all — the glob is wrong")

    web = ROOT / "web"
    # Missing equipment is a refusal, not a verdict. Without this, an absent
    # `node` came out as a FileNotFoundError traceback and exit 1 — which
    # reads as "the port disagrees" when what happened is that nothing was
    # compared. 2 is this repository's exit code for "could not check".
    if shutil.which("node") is None:
        print("check_md_port: REFUSED — no `node` on PATH, so the TypeScript "
              "renderer cannot run and nothing was compared")
        sys.exit(2)
    if not (web / "node_modules").exists():
        print("check_md_port: REFUSED — web/node_modules absent, so nothing "
              "was compared; run npm install in web/")
        sys.exit(2)

    with tempfile.TemporaryDirectory() as tmp:
        tmpd = pathlib.Path(tmp)
        (web / "_md_harness.mjs").write_text(HARNESS)
        (tmpd / "files.json").write_text(json.dumps(files))
        try:
            # Node runs the TypeScript directly (type stripping); no build
            # step, so this gate cannot pass against a stale bundle.
            r = subprocess.run(
                ["node", "--experimental-strip-types", "_md_harness.mjs",
                 str(tmpd / "files.json"), str(tmpd / "out.json")],
                cwd=web, capture_output=True, text=True,
            )
        finally:
            (web / "_md_harness.mjs").unlink(missing_ok=True)
        if r.returncode != 0:
            print("check_md_port: the TypeScript renderer failed to run")
            print(r.stderr[-2000:])
            sys.exit(1)
        ts = json.loads((tmpd / "out.json").read_text())

    bad = []
    for f in files:
        py_html, py_toc = render(pathlib.Path(f).read_text(encoding="utf-8"))
        got = ts.get(f)
        if got is None:
            bad.append(f"{f}: the TypeScript renderer produced nothing")
            continue
        if got["html"] != py_html:
            # Report the first differing line, not the whole file: a diff of
            # two 40 KB strings is not a diagnosis.
            a, b = py_html.split("\n"), got["html"].split("\n")
            for i in range(max(len(a), len(b))):
                x = a[i] if i < len(a) else "<missing>"
                y = b[i] if i < len(b) else "<missing>"
                if x != y:
                    rel = pathlib.Path(f).relative_to(ROOT)
                    bad.append(f"{rel}: line {i + 1}\n      py: {x[:150]}\n      ts: {y[:150]}")
                    break
        py_toc_j = [{"level": l, "slug": s, "text": t} for l, s, t in py_toc]
        if got["toc"] != py_toc_j:
            rel = pathlib.Path(f).relative_to(ROOT)
            bad.append(f"{rel}: table of contents differs")

    if bad:
        print(f"check_md_port: FAIL — {len(bad)} of {len(files)} files differ\n")
        for b in bad[:12]:
            print(f"  {b}")
        if len(bad) > 12:
            print(f"  … and {len(bad) - 12} more")
        print("\nThe anchors these produce are what every internal link points at.")
        sys.exit(1)

    print(f"ok: {len(files)} markdown files render identically in both implementations")


if __name__ == "__main__":
    main()
