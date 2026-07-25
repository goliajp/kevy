#!/usr/bin/env bash
# embeddedgate C# driver. Builds the release kevy cdylib, points Kevy.Embedded
# at it via KEVY_FFI_LIB, restores LightningDB from nuget, runs Release.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

cargo build --release -p kevy-ffi --manifest-path "$ROOT/Cargo.toml" >/dev/null
ext=dylib; [ "$(uname)" = "Linux" ] && ext=so

export KEVY_FFI_LIB="$ROOT/target/release/libkevy_ffi.$ext"
cd "$HERE"
dotnet run -c Release --project embeddedgate.csproj
