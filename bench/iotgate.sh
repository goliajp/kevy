#!/usr/bin/env bash
# iotgate — IoT resource budgets for the kevy-embedded `core` archetype
# (Linux-class IoT: Pi Zero 2 / OpenWrt / industrial ARM).
#
# WHAT IS MEASURED, AND WHY IT CHANGED (2026-07-12)
#
# This gate used to size a workspace `--example`. That was the wrong
# artifact: building an example pulls the crate's dev-dependencies, and
# kevy-embedded dev-depends on the `kevy` server crate, which drags the
# whole server stack into the compile. The example measured ~655 KB
# while a REAL consumer — a standalone crate whose only dependency is
# kevy-embedded — gets ~411 KB on the same profile. The gate was
# reporting a number 60% larger than anything a user would ship.
#
# It now sizes `bench/iot-consumer`, a fixture deliberately kept OUTSIDE
# the workspace with no dev-dependencies. That IS the consumer shape.
#
# Budgets (ratchet — raising one needs a written verdict):
#   1. size (host framing, DARWIN-ONLY gate): the consumer built for the
#      host — aarch64 Mach-O, the dev-loop number        ≤ 550 KB
#   2. size (musl framing): the static-musl consumer, the form that
#      ships to a device (x86_64 measured 454 KB)        ≤ 600 KB
#   3. RSS: empty-store resident set right after open, measured on the
#      static-musl binary (aarch64 measured 336 KB)      ≤ 2048 KB
#
# Framing note: binary size is framing-dependent — an aarch64 Mach-O and
# an x86_64 glibc ELF differ by ~160 KB of libc/loader framing for
# byte-identical Rust. So budget 1 gates only on darwin, where it is the
# sole size signal and the framing is stable; budgets 2+3 are the
# enforced Linux face, measured on the real delivery artifact.
#
# Cross targets (aarch64 / armv7 / ARMv6 musl, thumbv7em no_std) stay
# compile-gated in CI's iot job.
set -u
cd "$(dirname "$0")/.."

FIXTURE="bench/iot-consumer"
BIN_NAME="kevy-iot-consumer"

BUDGET_HOST_KB=550
BUDGET_MUSL_KB=600
BUDGET_RSS_KB=2048

FAIL=0
IS_LINUX=$([ "$(uname -s)" = "Linux" ] && echo 1 || echo 0)

echo "== iotgate: build the core-tier CONSUMER (standalone, no dev-deps) =="
( cd "$FIXTURE" && cargo build --quiet --release --no-default-features --features core ) \
  || { echo "iotgate FAIL: host build"; exit 1; }
HBIN="$FIXTURE/target/release/$BIN_NAME"
[ -f "$HBIN" ] || { echo "iotgate FAIL: $HBIN missing"; exit 1; }
HOST_KB=$(( $(wc -c < "$HBIN") / 1024 ))

if [ "$IS_LINUX" = "1" ]; then
  echo "core consumer (host, informational): ${HOST_KB} KB"
else
  echo "core consumer (host): ${HOST_KB} KB (budget ${BUDGET_HOST_KB} KB)"
  if [ "$HOST_KB" -gt "$BUDGET_HOST_KB" ]; then
    echo "iotgate FAIL: host binary ${HOST_KB} KB > ${BUDGET_HOST_KB} KB"
    FAIL=1
  fi
fi

if [ "$IS_LINUX" = "1" ]; then
  MUSL_TARGET="x86_64-unknown-linux-musl"
  if ! rustup target list --installed | grep -q "^${MUSL_TARGET}$"; then
    echo "iotgate FAIL: ${MUSL_TARGET} not installed (rustup target add ${MUSL_TARGET})"
    FAIL=1
  else
    echo "== iotgate: build the static-musl consumer (the shipped form) =="
    if ( cd "$FIXTURE" && cargo build --quiet --release \
           --no-default-features --features core --target "$MUSL_TARGET" ); then
      MBIN="$FIXTURE/target/${MUSL_TARGET}/release/$BIN_NAME"
      MUSL_KB=$(( $(wc -c < "$MBIN") / 1024 ))
      echo "core consumer (musl): ${MUSL_KB} KB (budget ${BUDGET_MUSL_KB} KB)"
      if [ "$MUSL_KB" -gt "$BUDGET_MUSL_KB" ]; then
        echo "iotgate FAIL: musl binary ${MUSL_KB} KB > ${BUDGET_MUSL_KB} KB"
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
