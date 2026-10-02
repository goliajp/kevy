#!/usr/bin/env python3
"""Write crates/kevy/src/command_specs.rs from a running Redis.

For every command in kevy's VERB_META that Redis also has, ask the Redis
for its COMMAND INFO row - arity, flags, key positions, ACL categories,
tips, key specs and subcommands - and write them out as Rust data. The
Redis must be the version bench/COMPETITOR-ANCHORS.json pins; the script
asks the server and refuses any other.

    python3 tools/gen_command_specs.py [--port 6379] [--check]

--check compares instead of writing, and fails when the file differs.
"""

import argparse
import glob
import json
import pathlib
import re
import socket
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "kevy" / "src" / "command_specs.rs"
ANCHORS = ROOT / "bench" / "COMPETITOR-ANCHORS.json"


class Redis:
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port))
        self.file = self.sock.makefile("rb")

    def call(self, *args):
        frame = b"*%d\r\n" % len(args)
        for a in args:
            a = a.encode()
            frame += b"$%d\r\n%s\r\n" % (len(a), a)
        self.sock.sendall(frame)
        return self.read()

    def read(self):
        line = self.file.readline()
        kind, body = line[:1], line[1:-2]
        if kind == b"*":
            n = int(body)
            return None if n < 0 else [self.read() for _ in range(n)]
        if kind == b"$":
            n = int(body)
            return None if n < 0 else self.file.read(n + 2)[:-2].decode()
        if kind == b":":
            return int(body)
        if kind == b"-":
            raise RuntimeError(body.decode())
        return body.decode()


def verbs():
    names = []
    for path in sorted(glob.glob(str(ROOT / "crates/kevy/src/verb_meta/*.rs"))):
        names += re.findall(r'^    v\("([A-Z][A-Z0-9._|-]*)"', open(path).read(), re.M)
    return names


def pairs(flat):
    return dict(zip(flat[0::2], flat[1::2]))


def rust_str(s):
    return json.dumps(s)


def rust_strs(items):
    return "&[" + ", ".join(rust_str(i) for i in items) + "]"


def key_spec(raw):
    spec = pairs(raw)
    begin, find = pairs(spec["begin_search"]), pairs(spec["find_keys"])
    bs, fs = pairs(begin["spec"]), pairs(find["spec"])
    if begin["type"] == "index":
        b = "Begin::Index(%d)" % bs["index"]
    elif begin["type"] == "keyword":
        b = "Begin::Keyword(%s, %d)" % (rust_str(bs["keyword"]), bs["startfrom"])
    else:
        b = "Begin::Unknown"
    if find["type"] == "range":
        f = "Find::Range(%d, %d, %d)" % (fs["lastkey"], fs["keystep"], fs["limit"])
    elif find["type"] == "keynum":
        f = "Find::KeyNum(%d, %d, %d)" % (fs["keynumidx"], fs["firstkey"], fs["keystep"])
    else:
        f = "Find::Unknown"
    return "k(%s, %s, %s, %s)" % (rust_str(spec.get("notes", "")), rust_strs(spec["flags"]), b, f)


def row(info, subs):
    name, arity, flags, first, last, step, acl, tips, specs, _ = info
    keys = "&[" + ", ".join(key_spec(s) for s in specs) + "]"
    return "c(%s, %d, %s, %d, %d, %d, %s, %s, %s, %s)," % (
        rust_str(name), arity, rust_strs(flags), first, last, step,
        rust_strs(acl), rust_strs(tips), keys, subs,
    )


def render(port):
    r = Redis(port)
    pinned = json.load(open(ANCHORS))["anchors"]["redis"]["pinned"]
    version = pairs(r.call("HELLO", "2"))["version"]
    if version != pinned:
        sys.exit(f"gen_command_specs: REFUSED - the server is Redis {version}, the pin is {pinned}")
    infos = r.call("COMMAND", "INFO", *[v.lower() for v in verbs()])
    shared = sorted((i for i in infos if i is not None), key=lambda i: i[0])
    out = [
        "// CODEGEN: tools/gen_command_specs.py - Redis %s's COMMAND INFO for the commands" % pinned,
        "// kevy shares with it. Do not edit; regenerate against the pinned Redis.",
        "",
        "use super::command_spec::{Begin, CmdSpec, Find, c, k};",
        "",
    ]
    containers = []
    for info in shared:
        if info[9]:
            const = info[0].upper().replace("-", "_") + "_SUBS"
            containers.append((info[0], const))
            out.append("#[rustfmt::skip]")
            out.append("const %s: &[CmdSpec] = &[" % const)
            # in the order Redis lists them
            for sub in info[9]:
                out.append(row(sub, "&[]"))
            out.append("];")
            out.append("")
    out.append("/// Every shared command's row, sorted by name.")
    out.append("#[rustfmt::skip]")
    out.append("pub(super) static SPECS: &[CmdSpec] = &[")
    subs_of = dict(containers)
    for info in shared:
        out.append(row(info, subs_of.get(info[0], "&[]")))
    out.append("];")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=6379)
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    text = render(a.port)
    if a.check:
        if OUT.read_text() != text:
            sys.exit(f"gen_command_specs: STALE - {OUT.relative_to(ROOT)} differs from the server's COMMAND INFO")
        print("gen_command_specs: ok")
        return
    OUT.write_text(text)
    print(f"gen_command_specs: wrote {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
