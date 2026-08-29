#!/usr/bin/env python3
"""Did the release actually reach every door, or only the ones somebody remembered?

On 2026-08-30, an hour after v6.0.0 was reported published and verified
across "every channel", six doors were still serving old code:

    @goliapkg/kevy-node        5.1.0     five releases behind
    @goliapkg/kevy-ts          5.1.0
    @goliapkg/kevy-electron    5.1.0
    expo-kevy                  5.1.0
    react-native-kevy-nitro    5.1.0
    jp.golia:kevy              5.3.0     three behind
    tauri-plugin-kevy-api      404       documented as installable, never published

Nothing was broken. Each door had been published once, by hand, and then
left out of the tag workflow — which publishes crates.io, npm's wasm
package and the binaries, and nothing else. Every gate the repository owns
passed, because every gate reads the *tree*: the manifests all said 6.0.0,
the vendored engine bytes all said 6.0.0, the CI was green. The tree was
telling the truth about itself. No gate had ever asked a registry.

That is the whole gap this closes. The version check answers "do our files
agree with each other"; this one answers "does the world have what we said
we shipped".

The door list is DERIVED, never written down. A hand-kept list is how the
Java door was missed twice: it is a list, and a list is a thing somebody
has to remember to add to. Here, a directory under bindings/ with a
manifest IS a channel, and it starts being checked the moment it exists.
The exemptions below are the only hand-written part, and each one has to
say why.

It asks about the version in the newest release tag, not the working tree
— between releases the tree is supposed to be ahead, and a gate that is
permanently red is a gate nobody reads.

    python3 tools/check_channels_published.py            # newest v* tag
    python3 tools/check_channels_published.py 5.3.0      # any past release

Exit 0 every door has it · 1 a door is behind · 2 the run could not tell
"""

import json
import pathlib
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
UA = "kevy-channel-parity (https://github.com/goliajp/kevy)"

# A run that resolves fewer doors than this cannot have looked at the whole
# surface, whatever it found. Without a floor, one broken query reduces the
# gate to a smaller gate that still reports success — which is the shape of
# every instrument failure this repository has had.
FLOOR = 12

# Doors that exist in the tree and are deliberately NOT published. Each entry
# is a claim that its absence from a registry is intended.
NOT_PUBLISHED = {
    "bindings/tauri/tauri-plugin-kevy": (
        "path-dependency crate; its README installs it by path and says "
        "'or, once published' — publishing needs versions on the kevy-* "
        "path deps first, which is a decision, not an oversight"
    ),
    "bindings/android": "documentation door — points at jp.golia:kevy, ships nothing",
    "bindings/apple": "consumed by git URL; SwiftPM resolves the tag, not a registry",
    "bindings/cpp": "consumed by CMake FetchContent against the tag; no registry",
}

# Sample and smoke projects. They carry a version because their manifest
# format demands one, not because anybody installs them.
DEMO = ("example/", "barern-example/", "smoke/", "node_modules/", "/target/")


def demo(p: pathlib.Path) -> bool:
    s = str(p) + "/"
    return any(d in s for d in DEMO)


def released_version(argv) -> str:
    if len(argv) > 1:
        return argv[1]
    out = subprocess.run(
        ["git", "tag", "--list", "v[0-9]*", "--sort=-v:refname"],
        cwd=ROOT, capture_output=True, text=True,
    ).stdout.split()
    tags = [t for t in out if re.fullmatch(r"v\d+\.\d+\.\d+", t)]
    if not tags:
        sys.exit("check_channels_published: no release tag to ask about")
    return tags[0][1:]


def get(url: str, accept: str = None):
    """(status, body). status is None only when the request never got an answer.

    The first version of this collapsed "404, so it was never published" and
    "the network refused me" into the same None, and the gate reported it
    could not tell when what it had actually learned was that the package
    was absent. Distinguishing them is the whole difference between a
    finding and a refusal.
    """
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    if accept:
        req.add_header("Accept", accept)
    try:
        with urllib.request.urlopen(req, timeout=45) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, None
    except (urllib.error.URLError, OSError):
        return None, None


def jget(url: str, accept: str = None):
    """(status, parsed-or-None)."""
    st, b = get(url, accept)
    if b is None:
        return st, None
    try:
        return st, json.loads(b)
    except json.JSONDecodeError:
        return st, None


# ── one function per registry: given a name and a version, is it served? ──
# Each returns True, False, or None for "could not tell", which is a refusal
# and not a pass.

def on_npm(name, v):
    st, d = jget(f"https://registry.npmjs.org/{urllib.parse.quote(name, safe='')}")
    if st == 404:
        return False
    return None if d is None else (v in (d.get("versions") or {}))


def on_pypi(name, v):
    st, _ = get(f"https://pypi.org/pypi/{name}/{v}/json")
    if st == 200:
        return True
    return False if st == 404 else None


def on_nuget(name, v):
    st, d = jget(f"https://api.nuget.org/v3-flatcontainer/{name.lower()}/index.json")
    if st == 404:
        return False
    return None if d is None else (v in (d.get("versions") or []))


def on_maven(coord, v):
    group, artifact = coord.split(":")
    path = group.replace(".", "/")
    st, b = get(f"https://repo1.maven.org/maven2/{path}/{artifact}/{v}/{artifact}-{v}.pom")
    if st == 404:
        return False
    # By content: Central serves the pom, and the pom names the version.
    return None if b is None else (f"<version>{v}</version>".encode() in b)


def on_pub(name, v):
    st, d = jget(f"https://pub.dev/api/packages/{name}",
                 accept="application/vnd.pub.v2+json")
    if st == 404:
        return False
    return None if d is None else any(
        x.get("version") == v for x in (d.get("versions") or []))


def on_crates(name, v):
    # crates.io answers 403 to a request with no User-Agent; this session
    # nearly read one as "not published". The header is sent above.
    st, d = jget(f"https://crates.io/api/v1/crates/{name}/{v}")
    if st == 404:
        return False
    return None if d is None else (d.get("version", {}).get("num") == v)


def on_goproxy(module, v):
    st, b = get(f"https://proxy.golang.org/{module.lower()}/@v/v{v}.info")
    if st in (404, 410):
        return False
    return None if b is None else (f'"v{v}"'.encode() in b)


def on_ghcr(image, v):
    st, tok = jget(f"https://ghcr.io/token?scope=repository:{image}:pull&service=ghcr.io")
    if not tok or "token" not in tok:
        return None
    req = urllib.request.Request(
        f"https://ghcr.io/v2/{image}/manifests/{v}",
        headers={"User-Agent": UA, "Authorization": f"Bearer {tok['token']}",
                 "Accept": "application/vnd.oci.image.index.v1+json,"
                           "application/vnd.docker.distribution.manifest.list.v2+json,"
                           "application/vnd.docker.distribution.manifest.v2+json"})
    try:
        with urllib.request.urlopen(req, timeout=45) as r:
            return r.status == 200
    except urllib.error.HTTPError as e:
        return False if e.code == 404 else None
    except (urllib.error.URLError, OSError):
        return None


def on_github_release(repo, v):
    """A tag with no release behind it is a download page that 404s.

    `release.yml` builds the server binaries and writes the release notes in
    jobs separate from the publishing ones; either can fail while crates.io
    and npm succeed, leaving a tag that every other door agrees with and a
    Releases page with nothing on it.
    """
    st, d = jget(f"https://api.github.com/repos/{repo}/releases/tags/v{v}")
    if st == 404:
        return False
    if d is None:
        return None
    # By content: published, and carrying something to download.
    return not d.get("draft", True) and len(d.get("assets") or []) > 0


def on_dockerhub(repo, v):
    st, d = jget(f"https://hub.docker.com/v2/repositories/{repo}/tags/{v}")
    if st == 404:
        return False
    return None if d is None else (d.get("name") == v)


def workspace_version() -> str:
    m = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"',
                  (ROOT / "Cargo.toml").read_text(encoding="utf-8"), re.M)
    if not m:
        sys.exit("check_channels_published: no workspace version in Cargo.toml")
    return m.group(1)


def doors():
    """Every door the tree has, read off the tree, with the version it declares.

    A new binding directory with a manifest becomes a checked channel with
    no edit here. That is the point: the doors this gate exists because of
    were missed by a human keeping a list, twice.

    The declared version matters because two crates are deliberately on
    their own line (kevy-client, kevy-client-async at 2.x). Asking those for
    the release version finds nothing and says so, which reads like a
    finding and is not one. Each door is asked about the version it claims.
    """
    out = []
    exempt = {ROOT / k for k in NOT_PUBLISHED}

    def excused(p):
        return any(x in p.parents or x == p for x in exempt)

    def add(kind, name, ask, src, declared):
        out.append((kind, name, ask, src, declared))

    for p in sorted(ROOT.glob("bindings/**/package.json")):
        if demo(p) or excused(p.parent):
            continue
        d = json.loads(p.read_text(encoding="utf-8"))
        if d.get("private") or not d.get("name"):
            continue
        add("npm", d["name"], on_npm, p, d.get("version"))

    for p in sorted(ROOT.glob("bindings/**/pyproject.toml")):
        if demo(p) or excused(p.parent):
            continue
        t = p.read_text(encoding="utf-8")
        m = re.search(r'^name\s*=\s*"([^"]+)"', t, re.M)
        vm = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', t, re.M)
        if m:
            add("pypi", m.group(1), on_pypi, p, vm.group(1) if vm else None)

    for p in sorted(ROOT.glob("bindings/**/pom.xml")):
        if demo(p) or excused(p.parent):
            continue
        t = p.read_text(encoding="utf-8")
        g = re.search(r"<groupId>([\w.]+)</groupId>", t)
        a = re.search(r"<artifactId>([\w.-]+)</artifactId>", t)
        vm = re.search(r"<version>(\d+\.\d+\.\d+)</version>", t)
        if g and a:
            add("maven", f"{g.group(1)}:{a.group(1)}", on_maven, p,
                vm.group(1) if vm else None)

    for p in sorted(ROOT.glob("bindings/**/*.csproj")):
        if demo(p) or excused(p.parent):
            continue
        t = p.read_text(encoding="utf-8")
        if re.search(r"<IsPackable>\s*false", t, re.I):
            continue
        pid = re.search(r"<PackageId>([\w.-]+)</PackageId>", t)
        vm = re.search(r"<Version>(\d+\.\d+\.\d+)</Version>", t)
        if vm:
            add("nuget", pid.group(1) if pid else p.stem, on_nuget, p, vm.group(1))

    for p in sorted(ROOT.glob("bindings/**/pubspec.yaml")):
        if demo(p) or excused(p.parent):
            continue
        t = p.read_text(encoding="utf-8")
        m = re.search(r"^name:\s*(\S+)", t, re.M)
        vm = re.search(r"^version:\s*(\d+\.\d+\.\d+)", t, re.M)
        if m:
            add("pub.dev", m.group(1), on_pub, p, vm.group(1) if vm else None)

    for p in sorted(ROOT.glob("bindings/**/go.mod")):
        if demo(p) or excused(p.parent):
            continue
        m = re.search(r"^module\s+(\S+)", p.read_text(encoding="utf-8"), re.M)
        if m:
            # A Go module carries no version of its own; the tag is the version.
            add("go", m.group(1), on_goproxy, p, None)

    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT, capture_output=True, text=True).stdout or "{}")
    for pk in sorted(meta.get("packages", []), key=lambda x: x["name"]):
        if pk.get("publish") == []:
            continue
        add("crates.io", pk["name"], on_crates,
            pathlib.Path(pk["manifest_path"]), pk.get("version"))

    # The GitHub release, with the repository read off the remote rather
    # than written here.
    remote = subprocess.run(["git", "remote", "get-url", "origin"],
                            cwd=ROOT, capture_output=True, text=True).stdout.strip()
    m = re.search(r"github\.com[:/]([\w.-]+/[\w.-]+?)(?:\.git)?$", remote)
    if m:
        add("github", m.group(1), on_github_release, ROOT / ".github/workflows/release.yml", None)

    # The container images, read out of the workflow that pushes them so the
    # names cannot drift apart from what actually gets tagged.
    dw = ROOT / ".github/workflows/docker.yml"
    if dw.exists():
        t = dw.read_text(encoding="utf-8")
        g = re.search(r"ghcr\.io/([\w.-]+/[\w.-]+)", t)
        h = re.search(r"docker\.io/([\w.-]+/[\w.-]+)", t)
        if g:
            add("ghcr", g.group(1), on_ghcr, dw, None)
        if h:
            add("dockerhub", h.group(1), on_dockerhub, dw, None)
    return out


def main() -> int:
    v = released_version(sys.argv)
    ws = workspace_version()
    ds = doors()
    if len(ds) < FLOOR:
        print(f"check_channels_published: found only {len(ds)} doors in the tree, "
              f"expected at least {FLOOR} — the derivation is broken, and a "
              f"smaller gate that passes is worse than no gate")
        return 2

    # Every directory under bindings/ must produce a door or say why it
    # does not. Without this, the gate quietly skips a manifest format it
    # was never taught — which is the hole it was written to close, one
    # level up: a door added in a format nobody added support for would be
    # unchecked, and the gate would keep reporting all-green.
    covered = set()
    for _, _, _, src, _ in ds:
        for parent in [src] + list(src.parents):
            if parent.parent == ROOT / "bindings":
                covered.add(parent.name)
                break
    excused = {k.split("/")[1] for k in NOT_PUBLISHED if k.startswith("bindings/")}
    unknown_doors = sorted(
        d.name for d in (ROOT / "bindings").iterdir()
        if d.is_dir() and d.name not in covered and d.name not in excused
    )
    if unknown_doors:
        print("bindings/ directories this gate cannot see:")
        for d in unknown_doors:
            print(f"  bindings/{d} — no manifest format it recognises, and not "
                  f"listed in NOT_PUBLISHED")
        print("\nTeach it the format, or record why the door ships nothing. "
              "A gate that silently skips a door is the gate that was missing.")
        return 2

    behind, unknown, ok, own_line = [], [], 0, []
    for kind, name, ask, src, declared in ds:
        # A door on its own version line is asked about that line. Anything
        # tracking the workspace is asked about the release.
        want = declared if (declared and declared != ws) else v
        if want != v:
            own_line.append(f"{name} {want}")
        got = ask(name, want)
        rel = src.relative_to(ROOT) if ROOT in src.parents else src
        if got is None:
            unknown.append(f"{kind:<10} {name} — the registry gave no answer")
        elif got:
            ok += 1
        else:
            behind.append(f"{kind:<10} {name:<28} does not serve {want}   ({rel})")

    if unknown:
        print(f"cannot tell whether {v} reached every door:")
        for u in unknown:
            print(f"  {u}")
        print("\nThis run could not answer. It is not a pass.")
        return 2

    if behind:
        print(f"{len(behind)} door(s) do not have it:")
        for b in behind:
            print(f"  {b}")
        print(f"\n{ok} of {len(ds)} doors are current.")
        print("A release that reached most of its channels is not released — "
              "the manifests, the docs and the tag all name a version the "
              "world cannot install.")
        print("See .claude/skills/release/SKILL.md.")
        return 1

    kinds = ", ".join(sorted({k for k, _, _, _, _ in ds}))
    print(f"ok: all {ok} doors serve {v} ({kinds})")
    if own_line:
        print(f"     on their own line: {', '.join(sorted(own_line))}")
    if NOT_PUBLISHED:
        print(f"     deliberately unpublished: {', '.join(sorted(NOT_PUBLISHED))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
