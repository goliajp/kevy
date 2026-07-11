#!/usr/bin/env bash
# iotgate — IoT resource budgets for the kevy-embedded `core` archetype
# (Linux-class IoT: Pi Zero / OpenWrt / industrial ARM).
#
# Budgets (ratchet — raising one needs a written verdict):
#   1. size (host framing, DARWIN-ONLY gate): the `core`-archetype
#      example binary, `--profile iot` (opt-level z + fat LTO +
#      strip), host target — aarch64 Mach-O, the dev-loop
#      number (655 KB)                                       ≤ 700 KB
#   2. size (musl framing):   the same example as a static-musl
#      binary — the form that actually ships to an IoT root fs;
#      includes all of libc, so it sits ~300 KB above the host
#      framing (first Linux measurement: 940-963 KB)         ≤ 1024 KB
#   3. RSS: empty-store resident set right after open, measured
#      on the STATIC-MUSL binary (first measurement: 736 KB) ≤ 2048 KB
#
# Framing verdict (2026-07-12): the host-binary SIZE is framing-
# dependent — an aarch64 Mach-O (darwin, 655 KB) and an x86_64 glibc
# ELF (Linux, 815 KB) differ by ~160 KB of libc/loader framing for
# byte-identical Rust. So budget 1 GATES only on darwin, where it's
# the sole size signal and the framing is stable; on Linux the host
# number is printed but not gated (its budget is unreachable under
# ELF framing). Budgets 2+3 are the enforced Linux face — measured on
# x86_64-unknown-linux-musl (the real IoT delivery form, natively
# runnable on any Linux box). A glibc host binary's RSS (~2.8 MB
# empty-store) is dominated by dynamic-loader + glibc malloc-arena
# overhead that never ships to the device, so RSS is asserted only on
# the musl artifact. The size story is split into budgets 1+2 rather
# than pretending one number covers both framings.
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
FAIL=0
IS_LINUX=$([ "$(uname -s)" = "Linux" ] && echo 1 || echo 0)
# The host binary's size is framing-dependent: an aarch64 Mach-O
# (darwin, ~655 KB) and an x86_64 glibc ELF (Linux, ~815 KB) differ by
# ~160 KB of libc/loader framing for byte-identical Rust. So the
# host-size budget only GATES on darwin — where it's the only size
# signal (no in-tree musl cross-runner) and the framing is stable.
# On Linux the static-musl artifact below is the real IoT delivery
# form and carries the enforced size budget; the host number is
# printed for continuity but not gated (its budget is unreachable
# under ELF framing and would false-fail).
if [ "$IS_LINUX" = "1" ]; then
  echo "core example binary (host, informational): ${SIZE_KB} KB"
else
  echo "core example binary (host): ${SIZE_KB} KB (budget ${BUDGET_BIN_KB} KB)"
  if [ "$SIZE_KB" -gt "$BUDGET_BIN_KB" ]; then
    echo "iotgate FAIL: host binary ${SIZE_KB} KB > ${BUDGET_BIN_KB} KB"
    FAIL=1
  fi
fi

if [ "$IS_LINUX" = "1" ]; then
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
