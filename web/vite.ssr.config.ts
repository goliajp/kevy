import react from '@vitejs/plugin-react'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { defineConfig } from 'vite'

// A second build, for node rather than the browser: the reference pages are
// rendered by the same React components at build time, so the site has one
// component tree instead of a landing page and a lookalike doc template.
function workspaceVersion(): string {
  const toml = readFileSync(resolve(__dirname, '../Cargo.toml'), 'utf8')
  const m = toml.match(/^version = "(\d+\.\d+\.\d+)"/m)
  if (!m) throw new Error('no workspace version in ../Cargo.toml')
  return m[1]
}

export default defineConfig({
  plugins: [react()],
  define: { __KEVY_VERSION__: JSON.stringify(workspaceVersion()) },
  build: {
    ssr: true,
    outDir: '.ssr',
    emptyOutDir: true,
    rollupOptions: {
      input: {
        'entry-docs': resolve(__dirname, 'src/entry-docs.tsx'),
        md: resolve(__dirname, 'src/md.ts'),
      },
      output: { format: 'es', entryFileNames: '[name].js' },
    },
  },
})
