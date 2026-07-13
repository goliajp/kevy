#!/usr/bin/env bash
# Offline install smoke for @goliapkg/kevy-node: stage this host's platform
# package, pack both tarballs, install them into a scratch project via
# file: specs (no registry), then run the typed surface on BOTH runtimes.
# Proves the published layout resolves — loaders find the platform package,
# not the in-repo target/ fallback.
#
#   cargo build --release -p kevy-ffi -p kevy-napi
#   bash packaging/npm/smoke-node.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

# Non-interactive shells don't get nvm's lazy-loaded node/npm; fall back
# to the newest nvm install. CI (setup-node) has both on PATH already.
if ! command -v npm >/dev/null 2>&1; then
    nvm_bin=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
    [ -n "$nvm_bin" ] && export PATH="$nvm_bin:$PATH"
fi

os="$(uname -s | tr '[:upper:]' '[:lower:]')"; [ "$os" = darwin ] || os=linux
cpu="$(uname -m)"; case "$cpu" in aarch64|arm64) cpu=arm64;; x86_64) cpu=x64;; esac
ext=so; [ "$os" = darwin ] && ext=dylib

STAGE=$(mktemp -d /tmp/kevy-npm-node-XXXXXX)
trap 'rm -rf "$STAGE"' EXIT

bash packaging/npm/gen-node-platform-pkg.sh "$os" "$cpu" \
    "target/release/libkevy_napi.$ext" "target/release/libkevy_ffi.$ext" "$STAGE"

# Pack both packages into tarballs (what a registry would serve).
(cd "$STAGE/kevy-node-$os-$cpu" && npm pack --quiet --pack-destination "$STAGE" >/dev/null)
(cd bindings/node && npm pack --quiet --pack-destination "$STAGE" >/dev/null)

# A scratch consumer project; file: install, offline.
APP="$STAGE/app"
mkdir -p "$APP"
(cd "$APP" && npm init -y >/dev/null 2>&1 && npm install --no-audit --no-fund --quiet \
    "$STAGE"/goliapkg-kevy-node-4*.tgz "$STAGE"/goliapkg-kevy-node-"$os"-"$cpu"-*.tgz >/dev/null)

cat > "$APP/smoke.mjs" <<'EOF'
import { open, text } from "@goliapkg/kevy-node";
const db = await open();
db.set("k", "v");
if (db.getText("k") !== "v") { console.error("FAIL get"); process.exit(1); }
let got = "";
db.subscribe("c", (p) => { got = text(p); });
db.publish("c", "hi");
await new Promise((r) => setTimeout(r, 120));
if (got !== "hi") { console.error("FAIL pubsub"); process.exit(1); }
db.close();
console.log("ok");
EOF

echo "smoke-node: node…"
(cd "$APP" && node smoke.mjs)
echo "smoke-node: bun…"
(cd "$APP" && bun smoke.mjs)
echo "smoke-node: PASS ($os-$cpu, installed layout)"
