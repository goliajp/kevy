#!/usr/bin/env bash
# mcpgate — the MCP protocol e2e contract gate (v4 T4, K-404).
#
# aigate's MCP phase proves the session boots; this gate owns the full
# protocol contract: JSON-RPC framing (parse error / invalid request /
# method-not-found), the whole tool matrix (discover / read / write /
# explain / info) in BOTH gating modes (default read-only and
# --allow-writes), the verb-class rejections (write-via-read,
# read-via-write, blocking, pubsub, unknown), and error fidelity —
# server -ERR text must reach the client verbatim, code prefix
# included, as an isError:true tool result, never laundered into a
# protocol error.
#
# Usage: bash bench/mcpgate.sh <kevy-binary>   (kevy-mcp expected next to it)
set -u
KBIN=${1:?usage: mcpgate.sh <kevy-binary>}
KBIN=$(cd "$(dirname "$KBIN")" && pwd)/$(basename "$KBIN")
MCP=$(dirname "$KBIN")/kevy-mcp
if [ ! -x "$MCP" ]; then
    echo "mcpgate: FAIL — kevy-mcp binary missing next to $KBIN"
    exit 1
fi
PORT=7332
DIR=$(mktemp -d /tmp/kevy-mcpgate-XXXXXX)
SPID=""
trap 'kill $SPID 2>/dev/null; rm -rf "$DIR"' EXIT

env KEVY_BIND=127.0.0.1 "$KBIN" --threads 4 --port $PORT --dir "$DIR" --no-aof >/dev/null 2>&1 &
SPID=$!
for _ in $(seq 1 50); do
    (exec 3<>/dev/tcp/127.0.0.1/$PORT) 2>/dev/null && { exec 3>&- 2>/dev/null; break; }
    sleep 0.2
done

python3 - "$MCP" $PORT <<'PY'
import json, signal, subprocess, sys, time

MCP, PORT = sys.argv[1], sys.argv[2]
signal.alarm(180)  # hard stop: a wedged session must fail, not hang CI

fails = []
def clamp(name, ok, detail=""):
    print(f"mcpgate: {'ok' if ok else 'FAIL'} — {name}" + (f" ({detail})" if detail else ""))
    if not ok:
        fails.append(name)

class Session:
    def __init__(self, *extra):
        self.p = subprocess.Popen(
            [MCP, "--url", f"redis://127.0.0.1:{PORT}", *extra],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True)
        self.n = 0
    def send_raw(self, line):
        self.p.stdin.write(line + "\n"); self.p.stdin.flush()
    def recv(self):
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("kevy-mcp closed stdout")
        return json.loads(line)
    def rpc(self, method, params=None):
        self.n += 1
        f = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None:
            f["params"] = params
        self.send_raw(json.dumps(f))
        r = self.recv()
        if r.get("id") != self.n:
            raise RuntimeError(f"id mismatch: sent {self.n}, got {r.get('id')}")
        return r
    def call(self, tool, **args):
        return self.rpc("tools/call", {"name": tool, "arguments": args})
    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=10)

def text_of(r):        # tool result -> (text, isError)
    res = r.get("result", {})
    return res.get("content", [{}])[0].get("text", ""), res.get("isError")

# ───────── phase R: default session — read-only gating ─────────
S = Session()

init = S.rpc("initialize", {})["result"]
clamp("initialize: protocolVersion 2024-11-05", init.get("protocolVersion") == "2024-11-05")
clamp("initialize: tools capability + serverInfo name",
      "tools" in init.get("capabilities", {}) and init.get("serverInfo", {}).get("name") == "kevy-mcp")

# a notification gets NO reply frame — the next reply must belong to ping
S.send_raw(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}))
clamp("notification unanswered, ping returns {}", S.rpc("ping").get("result") == {})

tools = {t["name"]: t for t in S.rpc("tools/list")["result"]["tools"]}
clamp("tools/list (default): exactly discover/read/explain/info, no write",
      sorted(tools) == ["kevy_discover", "kevy_explain", "kevy_info", "kevy_read"])
clamp("every tool: description + object inputSchema",
      all(t.get("description") and t.get("inputSchema", {}).get("type") == "object"
          for t in tools.values()))

t, e = text_of(S.call("kevy_read", command=["PING"]))
clamp("kevy_read PING round-trips", t == '"PONG"' and e is False)
t, e = text_of(S.call("kevy_read", command=["GET", "mcpgate:missing"]))
clamp("kevy_read GET missing -> JSON null", t == "null" and e is False)

r = S.call("kevy_read", command=["SET", "a", "b"])
clamp("write verb via kevy_read rejected -> kevy_write hint",
      r.get("error", {}).get("code") == -32602 and "kevy_write" in r["error"]["message"])
r = S.call("kevy_read", command=["BLPOP", "x", "1"])
clamp("blocking verb excluded from whitelist", "error" in r and "BLPOP" in r["error"]["message"])
r = S.call("kevy_read", command=["SUBSCRIBE", "c"])
clamp("pubsub verb excluded from whitelist", "error" in r and "SUBSCRIBE" in r["error"]["message"])
r = S.call("kevy_read", command=["NOPE"])
clamp("unknown verb error points at kevy_discover",
      "error" in r and "kevy_discover" in r["error"]["message"])
r = S.call("kevy_write", command=["SET", "a", "b"])
clamp("kevy_write gated by default (-32602 writes disabled)",
      r.get("error", {}).get("code") == -32602 and "writes disabled" in r["error"]["message"])

t, e = text_of(S.call("kevy_discover"))
table = json.loads(t)
clamp("kevy_discover: full live verb table (>=180 verbs)", e is False and len(table) >= 180,
      f"n={len(table)}")
clamp("discover table rows carry syntax + write flag split",
      table.get("SET", {}).get("flags", []).count("write") == 1
      and table.get("GET", {}).get("syntax", "").startswith("GET"))
t, e = text_of(S.call("kevy_discover", verb="SET"))
clamp("kevy_discover single-verb form", e is False and list(json.loads(t)) == ["SET"])

t, e = text_of(S.call("kevy_info", section="server"))
clamp("kevy_info returns the INFO server section", e is False and "version" in t)

t, e = text_of(S.call("kevy_explain", index="nope", args=["RANGE", "0", "1"]))
clamp("kevy_explain missing index: verbatim -ERR as isError result",
      e is True and t.startswith("ERR") and "no such index" in t, t[:60])

# error fidelity through kevy_read: a server arity error must arrive
# verbatim (code prefix intact) as an isError tool result
t, e = text_of(S.call("kevy_read", command=["GET"]))
clamp("server arity -ERR reaches client verbatim (isError:true)",
      e is True and t.startswith("ERR"), t[:60])

r = S.call("no_such_tool")
clamp("unknown tool -> -32602", r.get("error", {}).get("code") == -32602)
r = S.rpc("bogus/method")
clamp("unknown method -> -32601", r.get("error", {}).get("code") == -32601)
r = S.call("kevy_read")
clamp("kevy_read without 'command' -> -32602",
      r.get("error", {}).get("code") == -32602 and "command" in r["error"]["message"])
r = S.call("kevy_read", command=[])
clamp("kevy_read empty argv -> -32602", r.get("error", {}).get("code") == -32602)
r = S.call("kevy_read", command=["GET", 7])
clamp("non-string argv item -> -32602", r.get("error", {}).get("code") == -32602)

S.send_raw("{this is not json")
r = S.recv()
clamp("parse error -> -32700 with null id",
      r.get("error", {}).get("code") == -32700 and r.get("id") is None)
S.send_raw(json.dumps({"jsonrpc": "2.0", "id": 999}))  # no method
r = S.recv()
clamp("invalid request -> -32600", r.get("error", {}).get("code") == -32600)
S.close()

# ───────── phase W: --allow-writes session — the write surface ─────────
W = Session("--allow-writes")
W.rpc("initialize", {})
tools = [t["name"] for t in W.rpc("tools/list")["result"]["tools"]]
clamp("tools/list (--allow-writes): kevy_write advertised, 5 tools",
      "kevy_write" in tools and len(tools) == 5)

t, e = text_of(W.call("kevy_write", command=["SET", "mg:k", "v1"]))
clamp("kevy_write SET succeeds", t == '"OK"' and e is False)
t, e = text_of(W.call("kevy_read", command=["GET", "mg:k"]))
clamp("written value reads back via kevy_read", t == '"v1"')
r = W.call("kevy_write", command=["GET", "mg:k"])
clamp("read verb via kevy_write rejected -> kevy_read hint",
      r.get("error", {}).get("code") == -32602 and "kevy_read" in r["error"]["message"])

t, e = text_of(W.call("kevy_write", command=["SET", "mg:only-key"]))
clamp("write-path arity -ERR verbatim (isError:true)", e is True and t.startswith("ERR"), t[:60])

for i in range(50):
    W.call("kevy_write", command=["HSET", f"mg:g:{i}", "v", str(i)])
t, e = text_of(W.call("kevy_write", command=[
    "IDX.CREATE", "mgi", "ON", "PREFIX", "mg:g:", "FIELD", "v", "TYPE", "i64", "KIND", "range"]))
clamp("IDX.CREATE through kevy_write", e is False)
deadline = time.time() + 30
ready = False
while time.time() < deadline and not ready:
    t, e = text_of(W.call("kevy_read", command=["IDX.QUERY", "mgi", "RANGE", "0", "9", "LIMIT", "1"]))
    ready = e is False
    if not ready:
        time.sleep(0.3)
clamp("index built: IDX.QUERY through kevy_read", ready)
t, e = text_of(W.call("kevy_explain", index="mgi", args=["RANGE", "0", "49", "LIMIT", "10"]))
clamp("kevy_explain returns the structured plan",
      e is False and "est_rows" in t and "plan" in t, t[:80])
W.close()

print("mcpgate: PASS" if not fails else f"mcpgate: FAIL — {fails}")
sys.exit(1 if fails else 0)
PY
exit $?
