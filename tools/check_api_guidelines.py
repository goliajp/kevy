#!/usr/bin/env python3
"""The public API against the mechanical half of the Rust API Guidelines.

Reads rustdoc's JSON for every publishable library crate (nightly only;
it builds it unless `--json-dir` names a directory that already holds it)
and checks what can be decided without judgment:

- struct-exhaustive: a struct with public fields that callers could build
  literally — adding a field to it is a breaking change (C-STRUCT-PRIVATE)
- enum-exhaustive: a public enum without `#[non_exhaustive]` — adding a
  variant is a breaking change
- bool-param: a `bool` parameter whose meaning the call site cannot show
  (C-CUSTOM-TYPE); a setter whose name names the flag, taking only it, is
  fine
- get-prefix: a method on `&self` named `get_…` (C-GETTER)
- trait-open: a public trait anyone may implement, so adding a method to it
  is breaking (C-SEALED). A trait is sealed when one of its supertraits is
  the crate's own and unreachable from the crate root (the sealed pattern:
  a `pub trait Sealed` in a private module). A supertrait from another
  crate (`Index`, `Send`) seals nothing, and an unreachable trait is not
  public API, so it is not reported
- missing-debug: a public type without `Debug` (C-DEBUG)
- error-impl: a type named `…Error` that is not a `std::error::Error`, or
  not `Send + Sync` (C-GOOD-ERR)
- string-error: a public function whose error is a `String` or `&str`,
  which a caller can neither match on nor chain as a source (C-GOOD-ERR)

`#[repr(C)]` and `#[repr(transparent)]` types are exempt from the first two:
their layout is the contract. Everything else a rule flags must be listed in
`suite/api-exemptions.toml` with the reason it is right as it is. An
exemption that no longer matches a finding fails too, so the list cannot
outlive what it excuses.

Floor rule: no crates, or no public items found, is a broken selector, not
a pass.

Run: python3 tools/check_api_guidelines.py [--json-dir DIR] [--list]
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
EXEMPTIONS = ROOT / "suite/api-exemptions.toml"
RULES = (
    "struct-exhaustive",
    "enum-exhaustive",
    "bool-param",
    "get-prefix",
    "trait-open",
    "missing-debug",
    "error-impl",
    "string-error",
)
# crates whose features exclude each other document with their defaults
DEFAULT_FEATURES_ONLY = {"kevy-client-async"}


def lib_crates() -> list[str]:
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=ROOT, capture_output=True, text=True, check=True,
        ).stdout
    )
    return sorted(
        p["name"]
        for p in meta["packages"]
        if p.get("publish") != [] and any("lib" in t["kind"] for t in p["targets"])
    )


def build_json(crates: list[str], out: pathlib.Path) -> None:
    target = ROOT / "target/api-json"
    out.mkdir(parents=True, exist_ok=True)
    for c in crates:
        feats = [] if c in DEFAULT_FEATURES_ONLY else ["--all-features"]
        r = subprocess.run(
            ["cargo", "+nightly", "rustdoc", "-q", "-p", c, "--lib", *feats, "--",
             "-Z", "unstable-options", "--output-format", "json", "--cap-lints", "warn"],
            cwd=ROOT, capture_output=True, text=True,
            env=dict(os.environ, CARGO_TARGET_DIR=str(target)),
        )
        if r.returncode != 0:
            sys.exit(f"check_api_guidelines: REFUSED — rustdoc JSON for {c} failed:\n{r.stderr[-2000:]}")
        name = c.replace("-", "_") + ".json"
        (out / name).write_bytes((target / "doc" / name).read_bytes())


def attrs(item: dict) -> list[str]:
    out = []
    for a in item.get("attrs") or []:
        if isinstance(a, str):
            out.append(a)
            continue
        for k, v in a.items():
            if k == "other":
                out.append(str(v))
            elif k == "repr" and isinstance(v, dict):
                # format 61 gives repr as {kind, int, ...}; spell it as source does
                kind = {"c": "C", "rust": None}.get(v.get("kind"), v.get("kind"))
                parts = [p for p in (kind, v.get("int")) if p]
                out.append(f"repr({', '.join(parts)})")
            else:
                out.append(k)
    return out


def layout_is_contract(item: dict) -> bool:
    return any("repr" in a and ("C" in a or "transparent" in a or "u8" in a) for a in attrs(item))


def non_exhaustive(item: dict) -> bool:
    return any("non_exhaustive" in a for a in attrs(item))


class Crate:
    def __init__(self, doc: dict):
        self.doc = doc
        self.index = doc["index"]
        self.paths = doc["paths"]

    def item(self, i):
        return self.index.get(str(i))

    def reachable(self) -> set[int]:
        """Ids nameable from the crate root: public modules' items and the
        targets of re-exports, transitively."""
        seen: set[int] = set()
        stack = [self.doc["root"]]
        while stack:
            i = stack.pop()
            if i in seen:
                continue
            seen.add(i)
            it = self.item(i)
            if not it:
                continue
            inner = it["inner"]
            if "module" in inner:
                stack.extend(inner["module"]["items"])
            elif "use" in inner and inner["use"].get("id") is not None:
                stack.append(inner["use"]["id"])
        return seen

    def public_path(self, i) -> str | None:
        p = self.paths.get(str(i))
        return "::".join(p["path"]) if p and p["crate_id"] == 0 else None

    def impls(self, ids):
        for i in ids:
            im = self.item(i)
            if im:
                yield im["inner"]["impl"]

    def trait_impls(self, ids) -> set[str]:
        return {im["trait"]["path"].split("::")[-1] for im in self.impls(ids) if im.get("trait")}

    def methods(self, ids):
        for im in self.impls(ids):
            if im.get("trait"):
                continue
            for mid in im["items"]:
                m = self.item(mid)
                if m and "function" in m["inner"] and m.get("visibility") == "public":
                    yield m


def fn_findings(owner: str, fn: dict):
    sig = fn["inner"]["function"]["sig"]
    name = fn["name"] or ""
    args = [(n, t) for n, t in sig["inputs"] if n != "self"]
    bools = [n for n, t in args if isinstance(t, dict) and t.get("primitive") == "bool"]
    setter = name.startswith(("with_", "set_")) and len(args) == 1
    if bools and not setter:
        yield "bool-param", f"{owner}{name}", f"bool parameter(s) {', '.join(bools)}"
    takes_self = bool(sig["inputs"]) and sig["inputs"][0][0] == "self"
    if name.startswith("get_") and takes_self:
        yield "get-prefix", f"{owner}{name}", "method named get_…"
    err = result_error(sig.get("output"))
    if err in ("String", "str"):
        yield "string-error", f"{owner}{name}", f"returns Result<_, {err}>"


def result_error(ty) -> str | None:
    """The error type's name when `ty` is a `Result<_, E>`."""
    rp = ty.get("resolved_path") if isinstance(ty, dict) else None
    if not rp or rp["path"].split("::")[-1] not in ("Result", "io::Result"):
        return None
    args = ((rp.get("args") or {}).get("angle_bracketed") or {}).get("args") or []
    if len(args) < 2 or "type" not in args[1]:
        return None
    e = args[1]["type"]
    if isinstance(e, dict) and "borrowed_ref" in e:
        e = e["borrowed_ref"]["type"]
    if isinstance(e, dict) and e.get("primitive") == "str":
        return "str"
    if isinstance(e, dict) and "resolved_path" in e:
        return e["resolved_path"]["path"].split("::")[-1]
    return None


def findings(docs: dict[str, dict]):
    for crate, doc in sorted(docs.items()):
        c = Crate(doc)
        reach = c.reachable()
        for iid, it in c.index.items():
            if it.get("crate_id") != 0 or it.get("visibility") != "public":
                continue
            path = c.public_path(iid)
            if not path:
                continue
            inner = it["inner"]
            if "struct" in inner or "enum" in inner:
                kind = "struct" if "struct" in inner else "enum"
                body = inner[kind]
                traits = c.trait_impls(body["impls"])
                if "Debug" not in traits:
                    yield "missing-debug", path, f"{kind} without Debug"
                if path.endswith("Error") and not {"Error", "Send", "Sync"} <= traits:
                    missing = sorted({"Error", "Send", "Sync"} - traits)
                    yield "error-impl", path, f"missing {', '.join(missing)}"
                if not layout_is_contract(it) and not non_exhaustive(it):
                    if kind == "enum":
                        yield "enum-exhaustive", path, f"{len(body['variants'])} variants"
                    else:
                        k = body["kind"] if isinstance(body["kind"], dict) else {}
                        fields = k.get("plain", {}).get("fields", []) if "plain" in k else [
                            f for f in k.get("tuple", []) if f is not None
                        ]
                        stripped = k.get("plain", {}).get("has_stripped_fields") or (
                            "tuple" in k and None in k["tuple"]
                        )
                        public = [f for f in fields if (c.item(f) or {}).get("visibility") == "public"]
                        if public and not stripped:
                            yield "struct-exhaustive", path, f"{len(public)} public fields"
                for m in c.methods(body["impls"]):
                    yield from fn_findings(f"{path}::", m)
            elif "trait" in inner:
                if int(iid) not in reach:
                    continue
                t = inner["trait"]
                sealed = any(
                    "trait_bound" in b
                    and (c.paths.get(str(b["trait_bound"]["trait"]["id"])) or {"crate_id": 0})["crate_id"] == 0
                    and b["trait_bound"]["trait"]["id"] not in reach
                    for b in t.get("bounds", [])
                )
                if not sealed:
                    yield "trait-open", path, "implementable outside the crate"
            elif "function" in inner:
                yield from fn_findings(f"{path.rsplit('::', 1)[0]}::" if "::" in path else "", it)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--json-dir", type=pathlib.Path)
    ap.add_argument("--list", action="store_true", help="print every finding, exempt or not")
    ap.add_argument("--only", help="comma-separated crates to check (default: all)")
    a = ap.parse_args()
    crates = lib_crates()
    if a.only:
        wanted = set(a.only.split(","))
        unknown = wanted - set(crates)
        if unknown:
            print(f"check_api_guidelines: REFUSED — not publishable library crates: {sorted(unknown)}")
            return 2
        crates = [c for c in crates if c in wanted]
    if not crates:
        print("check_api_guidelines: REFUSED — no publishable library crates found")
        return 2
    jd = a.json_dir or ROOT / "target/api-json/out"
    if not a.json_dir:
        build_json(crates, jd)
    docs = {}
    for c in crates:
        f = jd / (c.replace("-", "_") + ".json")
        if not f.exists():
            print(f"check_api_guidelines: REFUSED — no rustdoc JSON for {c} in {jd}")
            return 2
        docs[c] = json.loads(f.read_bytes())
    found = sorted(set(findings(docs)))
    # the floor is on what was looked at: a clean crate has no findings, a
    # broken selector has no public items
    items = sum(1 for d in docs.values() for p in d["paths"].values() if p["crate_id"] == 0)
    if not items:
        print("check_api_guidelines: REFUSED — found no public items at all")
        return 2
    table = tomllib.loads(EXEMPTIONS.read_text()) if EXEMPTIONS.exists() else {}
    exempt = {}
    for e in table.get("exempt", []):
        if e.get("rule") not in RULES or not e.get("item") or len(e.get("why", "")) < 20:
            print(f"check_api_guidelines: bad exemption {e} — needs a known rule, an item and a reason")
            return 1
        exempt[(e["rule"], e["item"])] = e["why"]
    keys = {(r, i) for r, i, _ in found}
    open_ = [(r, i, w) for r, i, w in found if (r, i) not in exempt]
    prefixes = tuple(c.replace("-", "_") + "::" for c in crates)
    stale = sorted(k for k in exempt if k not in keys and k[1].startswith(prefixes))
    if a.list:
        for r, i, w in found:
            print(f"{'exempt' if (r, i) in exempt else 'OPEN  '} {r:18} {i}  ({w})")
    for r, i, w in open_:
        print(f"  ✗ {r:18} {i}  ({w})")
    for r, i in stale:
        print(f"  ✗ stale exemption  {r} {i} — nothing matches it any more")
    counts = {r: sum(1 for x in open_ if x[0] == r) for r in RULES}
    summary = ", ".join(f"{r} {n}" for r, n in counts.items() if n)
    if open_ or stale:
        print(f"check_api_guidelines: FAIL — {len(open_)} open ({summary}), {len(stale)} stale exemption(s)")
        return 1
    print(f"check_api_guidelines: ok — {len(docs)} crates, {len(found)} findings all exempt with reasons")
    return 0


if __name__ == "__main__":
    sys.exit(main())
