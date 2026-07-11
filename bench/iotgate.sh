#!/usr/bin/env bash
# iotgate — IoT resource budgets for the kevy-embedded `core` archetype
# (Linux-class IoT: Pi Zero / OpenWrt / industrial ARM).
#
# Budgets (ratchet — raising one needs a written verdict):
#   1. size (host framing):   the `core`-archetype example binary,
#      `--profile iot` (opt-level z + fat LTO + strip), host target
#      — the dev-loop number, comparable across darwin/Linux  ≤ 700 KB
#   2. size (musl framing):   the same example as a static-musl
#      binary — the form that actually ships to an IoT root fs;
#      includes all of libc, so it sits ~300 KB above the host
#      framing (first Linux measurement: 963 KB)             ≤ 1024 KB
#   3. RSS: empty-store resident set right after open, measured
#      on the STATIC-MUSL binary (first measurement: 784 KB) ≤ 2048 KB
#
# Framing verdict (2026-07-12): budgets 2+3 are measured on
# x86_64-unknown-linux-musl, natively runnable on any Linux box. A
# glibc host binary's RSS (~2.8 MB empty-store) is dominated by
# dynamic-loader + glibc malloc-arena overhead that never ships to
# the device — asserting the RSS budget on it gates the wrong
# artifact. The earlier header claim that musl framing differs "by
# 10s of KB" was wrong (libc is static-linked: +~308 KB); the size
# story is therefore split into the two budgets above instead of
# pretending one number covers both framings.
#
# On non-Linux hosts budgets 2+3 are a loud SKIP (run this gate on a
# Linux box for the full verdict). The cross targets (aarch64/armv7
# musl + thumbv7em no_std) stay compile-gated in CI's iot job.
#
# qemu-user note: CI does NOT run the full test suite under qemu
# (runner qemu install cost; the check matrix already gates the compile
# face). The manual lx64 item is:
#   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
#     cargo test -p kevy-embedded --target aarch64-unknown-linux-musl
set -u
cd "$(dirname "$0")/.."

BUDGET_BIN_KB=700
BUDGET_MUSL_BIN_KB=1024
BUDGET_RSS_KB=2048

echo "== iotgate: build core-archetype example (--profile iot) =="
cargo build --quiet --profile iot -p kevy-embedded --example iot_core \
  --no-default-features --features core || { echo "iotgate FAIL: build"; exit 1; }

BIN="target/iot/examples/iot_core"
[ -f "$BIN" ] || { echo "iotgate FAIL: $BIN missing"; exit 1; }

SIZE_KB=$(( $(wc -c < "$BIN") / 1024 ))
echo "core example binary (host): ${SIZE_KB} KB (budget ${BUDGET_BIN_KB} KB)"
FAIL=0
if [ "$SIZE_KB" -gt "$BUDGET_BIN_KB" ]; then
  echo "iotgate FAIL: host binary ${SIZE_KB} KB > ${BUDGET_BIN_KB} KB"
  FAIL=1
fi

if [ "$(uname -s)" = "Linux" ]; then
  MUSL_TARGET="x86_64-unknown-linux-musl"
  if ! rustup target list --installed | grep -q "^${MUSL_TARGET}$"; then
    echo "iotgate FAIL: ${MUSL_TARGET} not installed" \
         "(rustup target add ${MUSL_TARGET})"
    FAIL=1
  else
    echo "== iotgate: build static-musl artifact =="
    if cargo build --quiet --profile iot -p kevy-embedded --example iot_core \
         --no-default-features --features core --target "$MUSL_TARGET"; then
      MBIN="target/${MUSL_TARGET}/iot/examples/iot_core"
      MSIZE_KB=$(( $(wc -c < "$MBIN") / 1024 ))
      echo "core example binary (musl): ${MSIZE_KB} KB (budget ${BUDGET_MUSL_BIN_KB} KB)"
      if [ "$MSIZE_KB" -gt "$BUDGET_MUSL_BIN_KB" ]; then
        echo "iotgate FAIL: musl binary ${MSIZE_KB} KB > ${BUDGET_MUSL_BIN_KB} KB"
        FAIL=1
      fi
      echo "== iotgate: empty-store RSS (musl artifact) =="
      RSS_KB=$("$MBIN" | sed -n 's/^rss_kb=//p' | head -1)
      if [ -z "${RSS_KB}" ]; then
        echo "iotgate FAIL: no rss_kb line from $MBIN"
        FAIL=1
      else
        echo "empty-store RSS: ${RSS_KB} KB (budget ${BUDGET_RSS_KB} KB)"
        if [ "$RSS_KB" -gt "$BUDGET_RSS_KB" ]; then
          echo "iotgate FAIL: RSS ${RSS_KB} KB > ${BUDGET_RSS_KB} KB"
          FAIL=1
        fi
      fi
    else
      echo "iotgate FAIL: musl build"
      FAIL=1
    fi
  fi
else
  echo "SKIP: musl size + RSS budgets need a Linux host — host-size-only verdict here"
fi

if [ "$FAIL" -ne 0 ]; then
  echo "iotgate: FAIL"
  exit 1
fi
echo "iotgate: PASS"
