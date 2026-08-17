#!/usr/bin/env bash
# sitegate — the site's whole verification, one command (v5.3 suite).
#
# What CI's site job does, runnable locally: build (which compiles the
# wasm engine from this checkout first), check the built output
# (versions, links, reproducibility — check.mjs), then serve it and open
# it in a real Chromium (verify.mjs: the engine answers, the scenarios
# run, the shell is consistent). Exists because "the site is fine" was
# claimed from memory for a whole release line while every page said the
# previous version.
#
# usage: sitegate.sh
set -u
cd "$(dirname "$0")/.."

PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")
SRV=""
trap '[ -n "$SRV" ] && kill $SRV 2>/dev/null; wait 2>/dev/null' EXIT

(cd web && npm run build) >/tmp/sitegate-build.log 2>&1 || {
  echo "sitegate: FAIL — the site did not build"
  tail -12 /tmp/sitegate-build.log
  exit 1
}

(cd web && node check.mjs) || exit 1

(cd web/dist && python3 -m http.server "$PORT" >/dev/null 2>&1) &
SRV=$!
for _ in $(seq 1 30); do
  python3 -c "
import socket,sys
try: socket.create_connection(('127.0.0.1',$PORT),timeout=1).close(); sys.exit(0)
except OSError: sys.exit(1)" && break
  sleep 1
done

(cd web && node verify.mjs "http://localhost:$PORT") || exit 1

echo "sitegate: PASS (built, checked, and opened in a browser)"
