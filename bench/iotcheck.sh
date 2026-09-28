#!/usr/bin/env bash
# iotcheck — the embedded cut compiles, and boots, on every board it claims.
#
# CI's iot job and the suite both run this file; nostdgate reads its no_std
# lines to run them first. Needs the musl and thumbv7em targets, the
# RISC-V cross gcc and qemu-system-arm; a missing one is a refusal.
set -euo pipefail
cd "$(dirname "$0")/.."

for t in aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf \
         x86_64-unknown-linux-musl riscv64gc-unknown-linux-musl thumbv7em-none-eabihf; do
  rustup target list --installed 2>/dev/null | grep -qx "$t" || {
    echo "iotcheck: REFUSED — $t is not installed (rustup target add $t)" >&2; exit 2; }
done
for tool in riscv64-linux-gnu-gcc qemu-system-arm; do
  command -v "$tool" >/dev/null || { echo "iotcheck: REFUSED — $tool is not installed" >&2; exit 2; }
done

# kevy-embedded's full surface on the Tier A musl targets. ARMv6
# (arm-unknown-linux-musleabihf) is the Pi Zero / Pi 1 class the docs name.
cargo check --target aarch64-unknown-linux-musl -p kevy-embedded
cargo check --target armv7-unknown-linux-musleabihf -p kevy-embedded
cargo check --target arm-unknown-linux-musleabihf -p kevy-embedded

# The no_std core stones. kevy-store pulls kevy-hash / -bytes / -map /
# -madvise with the same no-default feature set, so one check proves all
# five; kevy-madvise is also checked on its own (store reaches it only
# through kevy-map).
cargo check --target thumbv7em-none-eabihf -p kevy-store --no-default-features --features alloc,external-clock
cargo check --target thumbv7em-none-eabihf -p kevy-madvise --no-default-features --features alloc

# The six kevy-embedded feature archetypes.
for f in core core,persist core,index core,index,text,vector core,persist,replicate core,listener; do
  echo "== features: $f =="
  cargo check -p kevy-embedded --no-default-features --features "$f"
done

# The real delivery shape: a crate outside the workspace whose only
# dependency is kevy-embedded. A workspace example pulls dev-dependencies
# (the kevy server) and inflated the measured size by ~60%, so the consumer
# is what iotgate sizes and what must compile on every board claimed.
(
  cd bench/iot-consumer
  for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
           armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf; do
    echo "== consumer (core) on $t =="
    cargo build --release --no-default-features --features core --target "$t"
  done
  echo "== consumer (core) on riscv64gc-unknown-linux-musl =="
  CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_MUSL_LINKER=riscv64-linux-gnu-gcc \
  RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release --no-default-features --features core \
      --target riscv64gc-unknown-linux-musl
)

# kevy-store booted on a bare-metal Cortex-M4 under QEMU: vector table,
# bump allocator, panic handler, semihosting. An exit code alone does not
# prove the firmware ran — a lockup or a QEMU that never reached main can
# leave one behind — so demand the sentinel printed after the KV and TTL
# checks pass.
log=$(mktemp)
(cd bench/mcu-probe && cargo run --release 2>&1 | tee "$log")
grep -q '^== kevy-store RUNS on a bare-metal MCU ==$' "$log" || {
  echo "iotcheck: FAIL — the MCU probe did not reach its success sentinel"; rm -f "$log"; exit 1; }
rm -f "$log"
echo "iotcheck: PASS"
