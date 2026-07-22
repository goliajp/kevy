#!/usr/bin/env bash
# Pack the Kevy NuGet (engine cdylib under runtimes/<rid>/native) and
# prove the INSTALLED layout works: a scratch console app pulls the
# package from a local feed and runs set/get/reopen WITHOUT KEVY_FFI_LIB
# — the .NET RID graph must find the native lib on its own.
#
#   cargo build --release -p kevy-ffi
#   bash packaging/nuget/pack-and-smoke.sh
#
# CI/release stage all three RIDs (osx-arm64 / linux-x64 / linux-arm64)
# before the publish `dotnet pack`; locally this smokes the host's RID.
set -euo pipefail
cd "$(dirname "$0")/../.."

if ! command -v dotnet >/dev/null 2>&1; then
    export PATH="/opt/homebrew/opt/dotnet@8/bin:$PATH"
fi
export DOTNET_CLI_TELEMETRY_OPTOUT=1

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) rid=osx-arm64 ext=dylib ;;
    Linux-x86_64) rid=linux-x64 ext=so ;;
    Linux-aarch64) rid=linux-arm64 ext=so ;;
    *) echo "unsupported host" >&2; exit 1 ;;
esac

PKGDIR=bindings/csharp/Kevy.Embedded
mkdir -p "$PKGDIR/runtimes/$rid/native"
cp "target/release/libkevy_ffi.$ext" "$PKGDIR/runtimes/$rid/native/"

# realpath, because on macOS mktemp hands back /tmp/... while the path
# dotnet resolves the project to is /private/tmp/... — nuget.config's
# `local` source is then a string that does not match the feed dotnet
# actually sees, and restore fails with NU1301 "source doesn't exist"
# even though the .nupkg is right there. Canonicalise once, up front, so
# every path below agrees.
STAGE=$(cd "$(mktemp -d /tmp/kevy-nuget-XXXXXX)" && pwd -P)
trap 'rm -rf "$STAGE"' EXIT
dotnet pack "$PKGDIR" -c Release -o "$STAGE/feed" >/dev/null
# Fail where the cause is, not three steps later at restore.
ls "$STAGE"/feed/*.nupkg >/dev/null 2>&1 || {
    echo "nuget-smoke: FAIL — dotnet pack produced no .nupkg in $STAGE/feed" >&2
    exit 1
}

APP="$STAGE/app"
mkdir -p "$APP"
cat > "$APP/nuget.config" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="local" value="$STAGE/feed" />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
</configuration>
EOF
cat > "$APP/app.csproj" <<'EOF'
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Exe</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <Nullable>enable</Nullable>
    <ImplicitUsings>enable</ImplicitUsings>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Kevy" Version="4.0.0" />
  </ItemGroup>
</Project>
EOF
cat > "$APP/Program.cs" <<'EOF'
using Kevy.Embedded;

var dir = args[0];
using (var db = KevyDb.Open(dir))
{
    db.Set("k", "v");
    if (db.GetText("k") != "v") { Console.Error.WriteLine("FAIL get"); return 1; }
}
using (var db = KevyDb.Open(dir))
{
    if (db.GetText("k") != "v") { Console.Error.WriteLine("FAIL reopen"); return 1; }
}
Console.WriteLine("ok");
return 0;
EOF

# No KEVY_FFI_LIB on purpose: the runtimes/<rid>/native layout is the test.
(cd "$APP" && dotnet run -c Release -- "$STAGE/data")
echo "nuget-smoke: PASS ($rid, installed layout)"
