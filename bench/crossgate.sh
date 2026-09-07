#!/usr/bin/env bash
# crossgate — one log, two engines. The AOF a browser tab writes must
# replay in the native server, and the AOF the native server writes must
# replay in the wasm engine.
#
# Why this exists: `crates/kevy-wasm/src/lib.rs` has said for several
# releases that "a log stored by a browser tab replays in a native kevy
# just as well", and nothing checked it. It was true when this gate was
# written — verified by hand, both directions — which is exactly when a
# claim is cheapest to nail down and most likely to be left as prose. The
# property is not incidental: it is the one thing kevy can do that neither
# Redis (no browser form) nor SQLite (no server form) can, so a silent
# divergence in the record format would cost more than a feature.
#
#   bash bench/crossgate.sh [<kevy-binary>]
#
# Defaults to target/release/kevy. Needs node and a built
# crates/kevy-wasm/pkg/kevy.wasm.
#
# Exit: 0 = both directions replay, 1 = a direction lost data, 2 = refused
# (a prerequisite is missing; nothing was measured).
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
KBIN=${1:-$ROOT/target/release/kevy}
WASM=$ROOT/crates/kevy-wasm/pkg/kevy.wasm

refuse() { echo "crossgate: REFUSED — $1" >&2; exit 2; }
command -v node >/dev/null || refuse "no node"
[ -x "$KBIN" ] || refuse "no kevy binary at $KBIN (cargo build --release -p kevy --bin kevy)"
[ -f "$WASM" ] || refuse "no wasm at $WASM (npm run engine in web/, or wasm-pack)"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/crossgate-XXXXXX")
SRV_PID=""
cleanup() {
    # Kill the PID we spawned. Never `pkill -f` with an interpolated
    # pattern — see bench/INCIDENT-2026-07-perfgate-pkill-massacre.md.
    [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
    [ -n "$SRV_PID" ] && wait "$SRV_PID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

# The values are chosen to cross every record shape the two engines encode
# differently if they ever diverge: a plain string, a list (order matters),
# a sorted set (a float score), a hash (field order does not), and a set.
mkdir -p "$WORK/fwd"
node - "$WASM" "$WORK/fwd/aof-0.aof" <<'NODE' || refuse "the wasm engine could not produce a log"
const { readFileSync, writeFileSync } = require('node:fs');
(async () => {
  const e = (await WebAssembly.instantiate(readFileSync(process.argv[2]), {})).instance.exports;
  const te = new TextEncoder(), td = new TextDecoder();
  const h = e.kevy_open(1); // OPEN_CAPTURE_AOF
  if (!h) { console.error('kevy_open returned 0'); process.exit(1); }
  const cmd = (...args) => {
    const parts = args.map((a) => te.encode(a));
    const total = parts.reduce((n, p) => n + 4 + p.length, 0);
    const p = e.kevy_alloc(total);
    const dv = new DataView(e.memory.buffer);
    let o = p;
    for (const b of parts) { dv.setUint32(o, b.length, true); o += 4; new Uint8Array(e.memory.buffer).set(b, o); o += b.length; }
    const n = e.kevy_cmd(h, p, total);
    const out = new Uint8Array(e.memory.buffer).slice(e.kevy_out_ptr(h), e.kevy_out_ptr(h) + e.kevy_out_len(h));
    e.kevy_free(p, total);
    if (n < 0) { console.error('cmd failed: ' + td.decode(out)); process.exit(1); }
    return td.decode(out);
  };
  cmd('SET', 'cross:str', 'written by the wasm engine');
  cmd('LPUSH', 'cross:list', 'first', 'second');
  cmd('ZADD', 'cross:zset', '1.5', 'alice');
  cmd('HSET', 'cross:hash', 'field', 'value');
  cmd('SADD', 'cross:set', 'member');
  const len = e.kevy_aof_dump(h);
  if (len <= 0) { console.error('kevy_aof_dump returned ' + len); process.exit(1); }
  const img = new Uint8Array(e.memory.buffer).slice(e.kevy_out_ptr(h), e.kevy_out_ptr(h) + len);
  writeFileSync(process.argv[3], Buffer.from(img));
})();
NODE

port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }

# One shard, so the log the wasm engine wrote is read as-is rather than
# re-homed across sixteen: this gate is about the record format, not about
# the reshard path (which has its own tests).
P=$(port)
"$KBIN" --threads 1 --port "$P" --dir "$WORK/fwd" >"$WORK/fwd.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 50); do
    python3 -c "import socket,sys;s=socket.socket();s.settimeout(0.2);sys.exit(0 if s.connect_ex(('127.0.0.1',$P))==0 else 1)" && break
    sleep 0.1
done

FAIL=0
say() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ✓ $1"
    else
        echo "  ✗ $1: expected [$2], got [$3]"
        FAIL=1
    fi
}

echo "crossgate: wasm-written log -> native engine"
q() { python3 - "$P" "$@" <<'PY'
import socket, sys, time
port, args = int(sys.argv[1]), sys.argv[2:]
s = socket.create_connection(("127.0.0.1", port), timeout=5)
out = f"*{len(args)}\r\n".encode() + b"".join(
    f"${len(a)}\r\n{a}\r\n".encode() for a in args)
s.sendall(out)
time.sleep(0.25)
body = s.recv(65536).decode(errors="replace")
# Last line of a bulk/simple reply, or the integer/array payload joined.
parts = [p for p in body.split("\r\n") if p and not p[0] in "$*"]
print("|".join(parts))
PY
}
say "string"      "written by the wasm engine" "$(q GET cross:str)"
say "list order"  "second|first"               "$(q LRANGE cross:list 0 -1)"
say "zset score"  "1.5"                        "$(q ZSCORE cross:zset alice)"
say "hash field"  "value"                      "$(q HGET cross:hash field)"
say "set member"  ":1"                         "$(q SISMEMBER cross:set member)"
grep -q "replayed 5 commands" "$WORK/fwd.log" \
    && echo "  ✓ five records replayed clean" \
    || { echo "  ✗ the replay line does not say five records:"; grep -i aof "$WORK/fwd.log" | head -2; FAIL=1; }

kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; SRV_PID=""

echo "crossgate: native-written log -> wasm engine"
mkdir -p "$WORK/rev"
P=$(port)
"$KBIN" --threads 1 --port "$P" --dir "$WORK/rev" >"$WORK/rev.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 50); do
    python3 -c "import socket,sys;s=socket.socket();s.settimeout(0.2);sys.exit(0 if s.connect_ex(('127.0.0.1',$P))==0 else 1)" && break
    sleep 0.1
done
q SET back:str "written by the native engine" >/dev/null
q RPUSH back:list one two >/dev/null
q ZADD back:zset 2.5 bob >/dev/null
# A clean shutdown flushes the AOF; the gate reads the file, not the socket.
kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; SRV_PID=""

node - "$WASM" "$WORK/rev/aof-0.aof" <<'NODE'
const { readFileSync } = require('node:fs');
(async () => {
  const e = (await WebAssembly.instantiate(readFileSync(process.argv[2]), {})).instance.exports;
  const te = new TextEncoder(), td = new TextDecoder();
  const h = e.kevy_open(0);
  const stage = (bytes) => {
    const p = e.kevy_alloc(bytes.length);
    new Uint8Array(e.memory.buffer).set(bytes, p);
    return p;
  };
  const log = new Uint8Array(readFileSync(process.argv[3]));
  const p = stage(log);
  const n = e.kevy_aof_frame_in(h, p, log.length);
  e.kevy_free(p, log.length);
  const out = () => td.decode(new Uint8Array(e.memory.buffer)
      .slice(e.kevy_out_ptr(h), e.kevy_out_ptr(h) + e.kevy_out_len(h)));
  if (n < 0) { console.error('  ✗ the wasm engine refused the native log: ' + out()); process.exit(1); }
  const cmd = (...args) => {
    const parts = args.map((a) => te.encode(a));
    const total = parts.reduce((s, b) => s + 4 + b.length, 0);
    const q = e.kevy_alloc(total);
    const dv = new DataView(e.memory.buffer);
    let o = q;
    for (const b of parts) { dv.setUint32(o, b.length, true); o += 4; new Uint8Array(e.memory.buffer).set(b, o); o += b.length; }
    e.kevy_cmd(h, q, total);
    const r = out();
    e.kevy_free(q, total);
    return r;
  };
  let bad = 0;
  const say = (name, want, got) => {
    if (got.includes(want)) console.log('  ✓ ' + name);
    else { console.log(`  ✗ ${name}: expected [${want}], got [${JSON.stringify(got)}]`); bad = 1; }
  };
  say('string', 'written by the native engine', cmd('GET', 'back:str'));
  say('list order', 'one', cmd('LINDEX', 'back:list', '0'));
  say('zset score', '2.5', cmd('ZSCORE', 'back:zset', 'bob'));
  process.exit(bad);
})();
NODE
[ $? -eq 0 ] || FAIL=1

if [ "$FAIL" -eq 0 ]; then
    echo "crossgate: PASS — the same log replays in both engines, both directions"
else
    echo "crossgate: FAIL — a log did not survive the crossing" >&2
fi
exit "$FAIL"
