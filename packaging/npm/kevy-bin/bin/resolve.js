// Resolve the native binary shipped by this platform's optional package.
//
// The esbuild pattern: the meta package declares one optionalDependency per
// (os, cpu) pair; npm/yarn/pnpm/bun install exactly the one that matches,
// and this shim execs the real binary from it. No postinstall script, no
// network fetch at install time — the binary IS the package.

"use strict";

const { execFileSync } = require("node:child_process");

const PLATFORMS = {
  "linux x64": "@goliapkg/kevy-bin-linux-x64",
  "linux arm64": "@goliapkg/kevy-bin-linux-arm64",
  "darwin arm64": "@goliapkg/kevy-bin-darwin-arm64",
};

function binaryPath(name) {
  const key = `${process.platform} ${process.arch}`;
  const pkg = PLATFORMS[key];
  if (!pkg) {
    console.error(
      `kevy: no prebuilt binary for ${key}.\n` +
        `Supported: ${Object.keys(PLATFORMS).join(", ")}.\n` +
        `Build from source instead: cargo install kevy` +
        (name === "kevy-cli" ? " kevy-cli" : ""),
    );
    process.exit(1);
  }
  try {
    // require.resolve() only walks module files — an extensionless native
    // binary is invisible to it. Resolve the package.json, join from there.
    const path = require("node:path");
    const root = path.dirname(require.resolve(`${pkg}/package.json`));
    const bin = path.join(root, name);
    require("node:fs").accessSync(bin);
    return bin;
  } catch {
    console.error(
      `kevy: ${pkg} is not installed.\n` +
        `Your package manager skipped optional dependencies — reinstall ` +
        `without --no-optional / --omit=optional.`,
    );
    process.exit(1);
  }
}

function run(name) {
  const bin = binaryPath(name);
  try {
    execFileSync(bin, process.argv.slice(2), { stdio: "inherit" });
  } catch (e) {
    if (typeof e.status === "number") process.exit(e.status);
    throw e; // spawn failure, not a non-zero exit
  }
}

module.exports = { run };
