#!/usr/bin/env python3
"""Publish an npm package that carries the prebuilt engine, and prove npm serves it.

    publish-prebuilt.py <package.tgz> <version> [--dry-run]

expo-kevy and react-native-kevy-nitro ship ~105 MB of xcframework slices
and jniLibs that are build outputs, not committed files. On a runner
without them `npm pack` quietly produces a 200 KB package that declares
`ios` and `android` and contains neither, and npm accepts it. So the
tarball given here must carry an engine that reports <version>, and must
not be less than half of what npm already serves for the package.

When npm already has <version>, the published tarball is fetched and
compared with this one by scripts/compare-prebuilt-tree.py: the same
files, byte for byte outside the engine; inside it, the same paths and the
same self-reported version. Same package is a pass that publishes nothing;
a different one is a failure, since npm never lets a version change.

After a publish, npm is asked again, until it serves the version (it can
take minutes) and the served tarball compares equal to this one.

Exit 0 published or already published with this content, 1 refused or
different, 2 could not tell.
"""

import json
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
COMPARE = ROOT / "scripts/compare-prebuilt-tree.py"
REGISTRY = "https://registry.npmjs.org"
WAIT_SECONDS = 15 * 60


def packument(name):
    url = f"{REGISTRY}/{urllib.parse.quote(name, safe='@')}"
    try:
        with urllib.request.urlopen(url, timeout=60) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return {}
        raise


def unpack(tgz, into):
    with tarfile.open(tgz) as t:
        t.extractall(into, filter="tar")
    return pathlib.Path(into)


def engine_dirs(tree):
    """Where the prebuilt engine sits: every *.xcframework and jniLibs dir."""
    out = []
    for p in sorted(tree.rglob("*")):
        if p.is_dir() and (p.suffix == ".xcframework" or p.name == "jniLibs"):
            if not any(str(p).startswith(str(o) + "/") for o in out):
                out.append(p)
    return out


def same_as_served(meta, version, local_tree, engines, tmp):
    tarball = meta["versions"][version]["dist"]["tarball"]
    served = pathlib.Path(tmp) / "served.tgz"
    urllib.request.urlretrieve(tarball, served)
    served_tree = unpack(served, pathlib.Path(tmp) / "served")
    rel = [str(e.relative_to(local_tree)) for e in engines]
    return subprocess.run([sys.executable, str(COMPARE), str(served_tree),
                           str(local_tree), version, *rel]).returncode


def main(argv):
    if len(argv) < 3:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    # Lines in order with the npm and compare output they interleave with.
    sys.stdout.reconfigure(line_buffering=True)
    tgz = pathlib.Path(argv[1]).resolve()
    version, dry = argv[2], "--dry-run" in argv[3:]
    with tempfile.TemporaryDirectory() as tmp:
        local = unpack(tgz, pathlib.Path(tmp) / "local")
        pkg = json.loads((local / "package/package.json").read_text())
        name = pkg["name"]
        if pkg["version"] != version:
            print(f"✗ {tgz.name} is {name}@{pkg['version']}, not {version}")
            return 1
        engines = engine_dirs(local)
        kinds = {e.suffix or e.name for e in engines}
        if kinds != {".xcframework", "jniLibs"}:
            print(f"✗ {name}@{version}: the tarball has {sorted(kinds) or 'no engine'},"
                  f" not both an xcframework and jniLibs")
            return 1

        meta = packument(name)
        if version in (meta.get("versions") or {}):
            print(f"{name}@{version} is on npm — comparing it with this build")
            rc = same_as_served(meta, version, local, engines, tmp)
            if rc == 0:
                print(f"✓ already published {name}@{version}, content matches")
            else:
                print(f"✗ {name}@{version} on npm is not this package")
            return rc

        now = sum(f.stat().st_size for f in local.rglob("*") if f.is_file())
        latest = (meta.get("dist-tags") or {}).get("latest")
        prev = (meta.get("versions") or {}).get(latest, {}).get("dist", {}).get("unpackedSize")
        if prev and now * 2 < prev:
            print(f"✗ REFUSING {name}@{version}: npm serves {latest} at {prev} bytes"
                  f" unpacked, this is {now}. Something the published package has is"
                  f" missing here.")
            return 1
        print(f"{name}@{version}: {now} bytes unpacked (npm serves {latest} at {prev})")

        cmd = ["npm", "publish", str(tgz), "--access", "public"]
        if dry:
            cmd.append("--dry-run")
        if subprocess.run(cmd).returncode != 0:
            print(f"✗ npm publish {name}@{version} failed")
            return 1
        if dry:
            print(f"dry run: would publish {name}@{version} from {tgz.name}")
            return 0

        deadline = time.monotonic() + WAIT_SECONDS
        while True:
            meta = packument(name)
            if version in (meta.get("versions") or {}):
                break
            if time.monotonic() > deadline:
                print(f"✗ npm accepted {name}@{version} but does not serve it"
                      f" after {WAIT_SECONDS // 60} minutes")
                return 1
            time.sleep(30)
        rc = same_as_served(meta, version, local, engines, tmp)
        print(f"{'✓' if rc == 0 else '✗'} npm serves {name}@{version}"
              f"{', and it is this package' if rc == 0 else ', but not this package'}")
        return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv))
