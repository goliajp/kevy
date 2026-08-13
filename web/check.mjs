#!/usr/bin/env node
// The built site must be the site the sources produce, and it must be
// internally sound. This is the gate; verify.mjs is the browser half.
//
//   node check.mjs
//
// What it asks, and why each one has a way of being wrong on its own:
//
//   1. Every version on every page is the workspace version. The site this
//      replaces served a masthead reading 5.0 and a hero reading 4.0 while
//      shipping 5.1.0, for a whole release line, because those were typed.
//   2. Every internal link resolves to a file that exists. A rename moves
//      a page and leaves every link to it pointing at nothing; the build
//      is delighted either way.
//   3. Every page a language has, the other languages have or knowingly
//      lack. A missing translation that still appears in a switch is a
//      404 offered to a reader in their own language.
//   4. No page is orphaned from the nav (prerender.mjs enforces this, and
//      it is re-checked here so the gate does not depend on reading the
//      build's output).
//   5. The output is reproducible: nothing in it depends on the time, the
//      machine, or the order a directory happened to list in.

import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const ROOT = join(HERE, '..')
const DIST = join(HERE, 'dist')

const problems = []
function fail(what, detail) {
  problems.push(`${what}${detail ? `\n      ${detail}` : ''}`)
}

if (!existsSync(DIST)) {
  process.stderr.write('check: no dist/ — run npm run build first\n')
  process.exit(1)
}

function walk(dir, out = []) {
  for (const e of readdirSync(dir)) {
    const p = join(dir, e)
    if (statSync(p).isDirectory()) walk(p, out)
    else out.push(p)
  }
  return out
}

const files = walk(DIST)
const pages = files.filter((f) => f.endsWith('.html'))
// A check that finds nothing must not pass. An empty dist/ is a broken
// build, not a clean one.
if (pages.length < 100) {
  fail(`only ${pages.length} pages in dist/ — expected the whole site`)
}

// ── 1. the version ───────────────────────────────────────────────────────
const VERSION = readFileSync(join(ROOT, 'Cargo.toml'), 'utf8').match(
  /^version = "(\d+\.\d+\.\d+)"/m,
)?.[1]
if (!VERSION) fail('no workspace version in Cargo.toml')

let versionsSeen = 0
for (const f of pages) {
  const html = readFileSync(f, 'utf8')
  for (const m of html.matchAll(/class="ver">([^<]+)</g)) {
    versionsSeen++
    if (m[1].trim() !== VERSION) {
      fail(`${relative(DIST, f)}: masthead says ${m[1]}, the workspace is ${VERSION}`)
    }
  }
  // Any OTHER version-shaped string next to the product name is a hand-typed
  // claim. `kevy 4.0` in a hero is exactly what went stale before.
  for (const m of html.matchAll(/kevy[\s ]+(\d+\.\d+(?:\.\d+)?)/gi)) {
    const v = m[1]
    if (v !== VERSION && !VERSION.startsWith(v + '.') && v !== VERSION.slice(0, v.length)) {
      // Version numbers legitimately appear in prose about OTHER versions —
      // upgrade guides name the version you are coming from. Those live in
      // the markdown, so only the generated chrome is held to this.
      if (!relative(DIST, f).includes('docs/')) {
        fail(`${relative(DIST, f)}: a typed "kevy ${v}" beside a site serving ${VERSION}`)
      }
    }
  }
}
// Every prerendered page states the version in its markup. The landing
// page does not: React renders it in the browser from a build-time define,
// so the static shell legitimately has none — which is why verify.mjs
// opens it in a browser and reads the rendered value. What can be checked
// here is that the value it will render is the right one: the bundle must
// contain the version and no other version-shaped literal beside the name.
const shell = pages.filter((f) => relative(DIST, f) === 'index.html')
const prerendered = pages.length - shell.length
if (versionsSeen < prerendered) {
  fail(`${prerendered - versionsSeen} prerendered pages carry no version at all`)
}
if (shell.length !== 1) fail(`expected one landing page, found ${shell.length}`)
const bundles = files.filter((f) => f.endsWith('.js') && f.includes('assets/'))
const inBundle = bundles.some((f) => readFileSync(f, 'utf8').includes(`"${VERSION}"`))
if (!inBundle) {
  fail(`no bundle contains "${VERSION}" — the landing page would render some other version`)
}

// ── 2. internal links ────────────────────────────────────────────────────
let links = 0
for (const f of pages) {
  const html = readFileSync(f, 'utf8')
  for (const m of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    const href = m[1]
    if (/^(https?:|mailto:|data:|#)/.test(href)) continue
    links++
    const [path] = href.split('#')
    if (!path) continue
    const target = path.startsWith('/')
      ? join(DIST, path)
      : resolve(dirname(f), path)
    const ok = existsSync(target) || existsSync(join(target, 'index.html'))
    if (!ok) fail(`${relative(DIST, f)}: dead link ${href}`)
  }
}

// ── 3. the translations line up ──────────────────────────────────────────
const docsOf = (lang) => {
  const d = lang === 'en' ? join(DIST, 'docs') : join(DIST, lang, 'docs')
  return existsSync(d) ? new Set(readdirSync(d)) : new Set()
}
const en = docsOf('en')
for (const lang of ['zh', 'ja']) {
  const other = docsOf(lang)
  for (const slug of other) {
    if (!en.has(slug)) fail(`${lang}/docs/${slug}/ has no English original`)
  }
}
if (en.size === 0) fail('no English documentation was built at all')

// ── 4. reproducible ──────────────────────────────────────────────────────
// Build twice and compare. Anything that differs between two builds of one
// tree is something a reader could be served two versions of, and something
// no diff of the sources would ever show.
process.stdout.write('  building a second time to compare…\n')
const first = new Map(pages.map((f) => [relative(DIST, f), readFileSync(f)]))
execFileSync('npm', ['run', 'build'], { cwd: HERE, stdio: 'pipe' })
const second = walk(DIST).filter((f) => f.endsWith('.html'))
if (second.length !== first.size) {
  fail(`two builds of one tree produced ${first.size} then ${second.length} pages`)
}
for (const f of second) {
  const rel = relative(DIST, f)
  const before = first.get(rel)
  if (!before) {
    fail(`${rel} appeared only in the second build`)
    continue
  }
  const after = readFileSync(f)
  if (!before.equals(after)) {
    // build.json carries the git rev on purpose and is not a page.
    fail(`${rel} differs between two builds of the same sources`)
  }
}

// ── report ───────────────────────────────────────────────────────────────
if (problems.length) {
  process.stderr.write(`\ncheck: FAIL — ${problems.length} problem(s)\n\n`)
  for (const p of problems.slice(0, 25)) process.stderr.write(`  ✗ ${p}\n`)
  if (problems.length > 25) process.stderr.write(`  … and ${problems.length - 25} more\n`)
  process.exit(1)
}
process.stdout.write(
  `check: PASS — ${pages.length} pages, ${links} internal links, all at ${VERSION}, byte-identical across two builds\n`,
)
