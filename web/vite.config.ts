import react from '@vitejs/plugin-react'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { defineConfig } from 'vite'

// The version is read from the workspace manifest at build time and injected
// as a define. It is never typed into a page.
//
// The site this replaces typed it in two places and both went stale: the
// masthead said 5.0 and the hero eyebrow said 4.0, on a site serving 5.1.0.
// Nothing was lying on purpose — a version in prose is simply a copy that
// nobody's tooling owns. Now there is exactly one copy, upstream of every
// page, and tools/check_site_version.py fails the build if a rendered page
// disagrees with it.
function workspaceVersion(): string {
  const toml = readFileSync(resolve(__dirname, '../Cargo.toml'), 'utf8')
  const m = toml.match(/^version = "(\d+\.\d+\.\d+)"/m)
  if (!m) throw new Error('no workspace version in ../Cargo.toml')
  return m[1]
}

export default defineConfig({
  plugins: [react()],
  define: {
    __KEVY_VERSION__: JSON.stringify(workspaceVersion()),
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // The wasm engine is large and cached separately from the page shell;
    // keeping hashed filenames means a new engine invalidates only itself.
    assetsInlineLimit: 4096,
  },
})
