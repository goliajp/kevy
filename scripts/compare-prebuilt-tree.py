#!/usr/bin/env python3
"""Is a published package the package this tree would publish?

    compare-prebuilt-tree.py PUBLISHED GENERATED VERSION ENGINE_DIR...

PUBLISHED and GENERATED are directories (an unpacked tarball, a checkout
of a mirror's tag). Every file outside the ENGINE_DIRs must be
byte-identical, and the two trees must list the same paths. Inside an
ENGINE_DIR the paths must match and every binary on both sides must
self-report VERSION, but the bytes may differ: the prebuilt engine is
rebuilt on each run, and neither the Xcode archive step nor ad-hoc code
signing is reproducible, so comparing those bytes would call every
re-run of a release a mismatch.

Exit 0 same package, 1 different, 2 could not compare.
"""

import os
import re
import sys

# What `strings | grep -x 'X.Y.Z'` finds: a version standing alone between
# non-printable bytes, which is how the engine's C ABI reports itself.
VERSION_STRING = re.compile(rb"(?<![\x20-\x7e\t])(\d+\.\d+\.\d+)(?![\x20-\x7e\t])")


def listing(root):
    out = {}
    for d, dirs, files in os.walk(root):
        dirs[:] = [x for x in dirs if x != ".git"]
        for name in files + [x for x in dirs if os.path.islink(os.path.join(d, x))]:
            p = os.path.join(d, name)
            out[os.path.relpath(p, root)] = p
    return out


def content(p):
    if os.path.islink(p):
        return b"symlink:" + os.readlink(p).encode()
    with open(p, "rb") as f:
        return f.read()


def is_binary(b):
    return b.startswith((b"\x7fELF", b"!<arch>", b"\xca\xfe\xba\xbe", b"\xcf\xfa\xed\xfe"))


def main(argv):
    if len(argv) < 5:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    pub, gen, version, engines = argv[1], argv[2], argv[3], argv[4:]
    a, b = listing(pub), listing(gen)
    if not a or not b:
        print(f"compare: an empty tree ({len(a)} vs {len(b)} files) is not a comparison")
        return 2
    problems = []
    for rel in sorted(set(a) - set(b)):
        problems.append(f"only published: {rel}")
    for rel in sorted(set(b) - set(a)):
        problems.append(f"only generated: {rel}")
    engine_binaries = 0
    for rel in sorted(set(a) & set(b)):
        ca, cb = content(a[rel]), content(b[rel])
        if not any(rel == e or rel.startswith(e.rstrip("/") + "/") for e in engines):
            if ca != cb:
                problems.append(f"differs: {rel}")
            continue
        for side, c in (("published", ca), ("generated", cb)):
            if is_binary(c):
                found = {m.decode() for m in VERSION_STRING.findall(c)}
                if version not in found:
                    problems.append(f"{side} {rel} does not report {version} ({sorted(found)[:3]})")
        engine_binaries += is_binary(cb)
    if engines and engine_binaries == 0:
        problems.append(f"no engine binary under {' '.join(engines)} — nothing was compared there")
    if problems:
        for p in problems[:40]:
            print(f"    {p}")
        return 1
    print(f"    {len(b)} files: all outside the engine identical, "
          f"{engine_binaries} engine binaries report {version} on both sides")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
