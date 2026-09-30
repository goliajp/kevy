#!/usr/bin/env python3
"""Wait until pub.dev serves a version, and check the archive has the engine in it.

    wait-pub-dev.py <package> <version>

Asked by content: the version is listed, its archive downloads, and the
archive holds the xcframework and the jniLibs with every .so reporting
<version>. A flutter_kevy without them resolves and analyses cleanly and
fails at DynamicLibrary.open.

Gives up after 15 minutes. Exit 0 served with the engine, 1 not.
"""

import io
import json
import re
import sys
import tarfile
import time
import urllib.error
import urllib.request

WAIT_SECONDS = 15 * 60
VERSION_STRING = re.compile(rb"(?<![\x20-\x7e\t])(\d+\.\d+\.\d+)(?![\x20-\x7e\t])")


def served(package, version):
    req = urllib.request.Request(f"https://pub.dev/api/packages/{package}",
                                 headers={"Accept": "application/vnd.pub.v2+json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            d = json.load(r)
    except (urllib.error.URLError, OSError):
        return None
    for v in d.get("versions") or []:
        if v.get("version") == version:
            return v.get("archive_url")
    return None


def main(argv):
    package, version = argv[1], argv[2]
    deadline = time.monotonic() + WAIT_SECONDS
    while (url := served(package, version)) is None:
        if time.monotonic() > deadline:
            print(f"✗ pub.dev does not serve {package} {version} after "
                  f"{WAIT_SECONDS // 60} minutes.")
            print("  If kevy-flutter's publish run failed at the upload, pub.dev has not"
                  " been told to accept publishing from GitHub Actions for"
                  " goliajp/kevy-flutter (package Admin tab, tag pattern v{{version}})."
                  " Until it has, publish by hand from the tag:")
            print(f"    git clone --branch v{version} git@github.com:goliajp/kevy-flutter.git"
                  f" /tmp/kevy-flutter && cd /tmp/kevy-flutter && flutter pub publish")
            return 1
        time.sleep(30)

    with urllib.request.urlopen(url, timeout=120) as r:
        archive = tarfile.open(fileobj=io.BytesIO(r.read()))
    names = archive.getnames()
    problems = []
    if not any("kevy_ffi.xcframework/" in n for n in names):
        problems.append("no ios/kevy_ffi.xcframework")
    libs = [m for m in archive.getmembers() if "jniLibs/" in m.name and m.name.endswith(".so")]
    if not libs:
        problems.append("no jniLibs/*.so")
    for m in libs:
        found = {x.decode() for x in VERSION_STRING.findall(archive.extractfile(m).read())}
        if version not in found:
            problems.append(f"{m.name} does not report {version}")
    if problems:
        print(f"✗ pub.dev serves {package} {version}, but the archive is not the package:")
        for p in problems:
            print(f"    {p}")
        return 1
    print(f"✓ pub.dev serves {package} {version}: {len(names)} files, "
          f"the xcframework and {len(libs)} engine .so reporting {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
