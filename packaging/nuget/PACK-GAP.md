# NuGet package: the pack form is not built yet

`pack-and-smoke.sh` was written to prove the installed layout works, and
wiring it into CI is what surfaced that the layout does not exist. Recorded
here so the next attempt starts from the real state, not the assumed one.

## What the CI run showed

Both legs failed with `NU1301: the local source '…/feed' doesn't exist`.
That error was three steps downstream of the cause. A `ls …/feed/*.nupkg`
guard added to the script names it directly:

    dotnet pack produced no .nupkg

## Why pack produces nothing

The script packs `bindings/csharp/Kevy.Embedded`, which is
`<IsPackable>false</IsPackable>` with `PackageId=Kevy.Embedded.Internal` —
an internal P/Invoke door, deliberately not a package. `dotnet pack` on it
exits 0 and writes nothing.

The package that *is* meant to ship is `bindings/csharp/Kevy.Client`
(`PackageId=kevy`, packable). But it reaches the embedded engine through a
`<ProjectReference>` to that internal, non-packable door, and a
ProjectReference does not travel inside a NuGet package by default — it
becomes a PackageReference dependency on a package that will never be
published.

## So the real gap is a design one

For `kevy` to install and run `KevyDb.Open` without `KEVY_FFI_LIB`, the
package has to carry, itself:

- the `Kevy.Embedded` assembly (via a pack target that bundles the
  ProjectReference output, or by making Embedded a packable dependency), and
- the engine cdylib under `runtimes/<rid>/native/` for each shipped RID.

Neither is in place. This is publish-form design, which belongs to the t6
channel decision alongside the go `/vN` question — not a script fix.

## What does work today

The C# conformance suite is green because it loads the engine from the
checkout via `KEVY_FFI_LIB`, bypassing packaging entirely. That proves the
binding; it does not prove the package. The npm door, by contrast, does
prove its package — `npm-install-smoke` installs the built tarballs and
resolves the loader against the platform package, green on both CI legs.
NuGet needs the equivalent pack form before its smoke can mean the same
thing.
