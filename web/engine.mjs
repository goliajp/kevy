// The engine the site demonstrates is the engine in this checkout.
//
// It used to be whatever was last published to npm, which meant the
// landing page ran an engine nobody in this repository had built: the
// published 5.1.0 wasm was compiled `features = ["core", "persist"]`, so
// every IDX./VIEW./TABLE. verb on a page whose whole argument is secondary
// indexes came back "unknown command". A reader found it before any gate
// did, because no gate could — the package was correct for what it was,
// and the site was correct for what it asked for.
//
// So web/package.json depends on `file:../crates/kevy-wasm/pkg`, and this
// runs first to make sure the one file that is not checked in is there.
import { homedir } from 'node:os'
import { existsSync, statSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const pkg = join(here, '../crates/kevy-wasm/pkg')
const wasm = join(pkg, 'kevy.wasm')
const built = join(here, '../target/wasm32-unknown-unknown/release/kevy_wasm.wasm')

if (!existsSync(wasm) || (existsSync(built) && statSync(built).mtimeMs > statSync(wasm).mtimeMs)) {
  if (process.env.KEVY_NO_WASM_BUILD) {
    console.error(
      `engine: ${wasm} is missing or stale, and KEVY_NO_WASM_BUILD is set.\n` +
        `  cargo build -p kevy-wasm --target wasm32-unknown-unknown --release\n` +
        `  cp target/wasm32-unknown-unknown/release/kevy_wasm.wasm crates/kevy-wasm/pkg/kevy.wasm`,
    )
    process.exit(1)
  }
  console.log('engine: building the wasm from this checkout')
  execFileSync(
    'cargo',
    ['build', '-p', 'kevy-wasm', '--target', 'wasm32-unknown-unknown', '--release'],
    {
      cwd: join(here, '..'),
      stdio: 'inherit',
      // Remap the builder's home out of the artifact. This wasm is served
      // from a public website, and on a machine with the `rust-src`
      // component installed rustc resolves std's panic locations to the
      // local toolchain source rather than the `/rustc/<hash>/` form the
      // official builds carry — putting 25 copies of
      // `/Users/<name>/.rustup/...` inside the file. CI has no rust-src, so
      // it never showed there and the two artifacts differed by 4 KB for a
      // reason nobody had looked at. Who built a public file is not
      // something the file should say.
      env: {
        ...process.env,
        RUSTFLAGS: [process.env.RUSTFLAGS ?? '', `--remap-path-prefix=${homedir()}=~`]
          .join(' ')
          .trim(),
      },
    },
  )
  execFileSync('cp', [built, wasm])
}

const kb = Math.round(statSync(wasm).size / 1024)
console.log(`engine: crates/kevy-wasm/pkg/kevy.wasm — ${kb} KB`)
