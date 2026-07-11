#!/usr/bin/env bash
# iotgate — IoT resource budgets for the kevy-embedded `core` archetype
# (Linux-class IoT: Pi Zero / OpenWrt / industrial ARM).
#
# Budgets (ratchet — raising one needs a written verdict):
#   1. size:  the `core`-archetype example binary, `--profile iot`
#             (opt-level z + fat LTO + strip)          ≤ 700 KB
#   2. RSS:   empty-store resident set right after open ≤ 2 MB
#
# Target selection: the size budget is measured on the HOST binary —
# same Rust code, same profile; Mach-O vs static-musl framing differs
# by 10s of KB, inside the budget's margin. On a Linux host the same
# binary also runs and prints `rss_kb=<n>`, closing budget 2; elsewhere
# the RSS assertion is a loud SKIP (run this gate on a Linux box for
# the full verdict). The true static-musl artifact is compile-gated in
# CI's iot job (aarch64/armv7 musl + thumbv7em no_std check matrix).
#
# qemu-user note: CI does NOT run the full test suite under qemu
# (runner qemu install cost; the check matrix already gates the compile
# face). The manual lx64 item is:
#   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
#     cargo test -p kevy-embedded --target aarch64-unknown-linux-musl
set -u
cd "$(dirname "$0")/.."

BUDGET_BIN_KB=700
BUDGET_RSS_KB=2048

echo "== iotgate: build core-archetype example (--profile iot) =="
cargo build --quiet --profile iot -p kevy-embedded --example iot_core \
  --no-default-features --features core || { echo "iotgate FAIL: build"; exit 1; }

BIN="target/iot/examples/iot_core"
[ -f "$BIN" ] || { echo "iotgate FAIL: $BIN missing"; exit 1; }

SIZE_KB=$(( $(wc -c < "$BIN") / 1024 ))
echo "core example binary: ${SIZE_KB} KB (budget ${BUDGET_BIN_KB} KB)"
FAIL=0
if [ "$SIZE_KB" -gt "$BUDGET_BIN_KB" ]; then
  echo "iotgate FAIL: binary ${SIZE_KB} KB > ${BUDGET_BIN_KB} KB"
  FAIL=1
fi

if [ "$(uname -s)" = "Linux" ]; then
  echo "== iotgate: empty-store RSS =="
  RSS_KB=$("$BIN" | sed -n 's/^rss_kb=//p' | head -1)
  if [ -z "${RSS_KB}" ]; then
    echo "iotgate FAIL: no rss_kb line from $BIN"
    FAIL=1
  else
    echo "empty-store RSS: ${RSS_KB} KB (budget ${BUDGET_RSS_KB} KB)"
    if [ "$RSS_KB" -gt "$BUDGET_RSS_KB" ]; then
      echo "iotgate FAIL: RSS ${RSS_KB} KB > ${BUDGET_RSS_KB} KB"
      FAIL=1
    fi
  fi
else
  echo "SKIP: RSS budget needs a Linux host (/proc VmRSS) — size-only verdict here"
fi

if [ "$FAIL" -ne 0 ]; then
  echo "iotgate: FAIL"
  exit 1
fi
echo "iotgate: PASS"
