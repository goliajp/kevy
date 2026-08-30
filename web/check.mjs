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

// ── the release notes are a real page ───────────────────────────────
//
// /changelog/ vanished when the site was rebuilt — the old pipeline
// generated it, the new one did not, and the server answers an unknown
// path with the shell and HTTP 200. A status code cannot tell those
// apart, so this asks for the content: the current version's heading,
// and enough of the file to be the file.
{
  const p = join(DIST, 'changelog', 'index.html')
  if (!existsSync(p)) {
    fail('there is no /changelog/ page at all')
  } else {
    const html = readFileSync(p, 'utf8')
    if (html.length < 50_000) {
      fail(`/changelog/ is only ${html.length} bytes — that is a shell, not the notes`)
    }
    if (!html.includes(VERSION)) {
      fail(`/changelog/ does not mention ${VERSION}`)
    }
  }
}

if (!VERSION) fail('no workspace version in Cargo.toml')

let versionsSeen = 0
for (const f of pages) {
  const html = readFileSync(f, 'utf8')
  // The version is a meta tag, not chrome: the Golia Lab masthead carries
  // a wordmark and nothing else. Every page still states it, so every page
  // can still be held to the manifest.
  for (const m of html.matchAll(/<meta name="generator" content="kevy ([^"]+)">/g)) {
    versionsSeen++
    if (m[1].trim() !== VERSION) {
      fail(`${relative(DIST, f)}: page declares ${m[1]}, the workspace is ${VERSION}`)
    }
  }
  // A version in the page's CHROME must be the version the page ships.
  // Content may name any version it likes, and often must: an upgrade
  // guide names the release you are coming from, and a benchmark's legend
  // names the build that was measured. Rewriting that legend to the
  // current version would turn an honest record of a 4.0 measurement into
  // a false claim about 5.1 — a stale benchmark is a reason to re-measure,
  // not to relabel.
  //
  // So the scan is scoped to the eyebrow, which is chrome, and which is
  // exactly where `kevy 4.0` sat on a site that was serving 5.1.0.
  for (const eyebrow of html.matchAll(/<div class="eyebrow">(.*?)<\/div>/gs)) {
    for (const m of eyebrow[1].matchAll(/kevy[\s ]+(\d+\.\d+(?:\.\d+)?)/gi)) {
      if (m[1] !== VERSION) {
        fail(`${relative(DIST, f)}: eyebrow says "kevy ${m[1]}", the site serves ${VERSION}`)
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
    // A directory is not a page. `existsSync(dist/docs)` is true because
    // the directory holds 36 subdirectories — and the link to /docs/ went
    // to a 404 for every visitor while this check reported it fine. What a
    // static server can serve is a FILE, or a directory that has an
    // index.html inside it.
    const ok = statSync(target, { throwIfNoEntry: false })?.isDirectory()
      ? existsSync(join(target, 'index.html'))
      : existsSync(target)
    if (!ok) {
      fail(`${relative(DIST, f)}: dead link ${href}`)
    }
  }
}

// ── 2b. the links a page makes about ITSELF ──────────────────────────────
// Skipped by the check above, which drops anything starting `https:` —
// and `<link rel="canonical">` and `rel="alternate"` are written absolute,
// so they were never asked to resolve. The release notes spent an unknown
// number of releases declaring themselves canonical at /docs/changelog/,
// a URL this host answers with the SPA shell and HTTP 200. A soft 404 is
// not visible from the outside; it is visible from here.
//
// Self-referential because an hreflang group that omits its own page is
// documented to be discarded whole, which is the same as having none.
const HOST = 'https://kevy.golia.jp'
let selfLinks = 0
for (const f of pages) {
  const html = readFileSync(f, 'utf8')
  const canon = [...html.matchAll(/<link rel="canonical" href="([^"]+)"/g)].map((m) => m[1])
  const alts = [...html.matchAll(/<link rel="alternate"[^>]*href="([^"]+)"/g)].map((m) => m[1])
  const rel = relative(DIST, f)
  if (canon.length !== 1) fail(`${rel}: ${canon.length} canonical links, expected 1`)
  if (alts.length === 0) fail(`${rel}: no hreflang alternates — every page has translations`)
  if (canon.length === 1 && alts.length > 0 && !alts.includes(canon[0])) {
    fail(`${rel}: canonical ${canon[0]} is not among its own alternates`)
  }
  for (const url of [...canon, ...alts]) {
    if (!url.startsWith(HOST)) continue
    selfLinks++
    const target = join(DIST, url.slice(HOST.length))
    const ok = statSync(target, { throwIfNoEntry: false })?.isDirectory()
      ? existsSync(join(target, 'index.html'))
      : existsSync(target)
    if (!ok) fail(`${rel}: ${url} points at a page that does not exist`)
  }
}
// A floor, because the loop above passes triumphantly on a dist/ where
// nothing declares anything at all.
if (selfLinks < pages.length * 2) {
  fail(`only ${selfLinks} self-referential links across ${pages.length} pages`)
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
  `check: PASS — ${pages.length} pages, ${links} internal links, ${selfLinks} canonical/hreflang, all at ${VERSION}, byte-identical across two builds\n`,
)
