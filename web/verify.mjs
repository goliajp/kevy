#!/usr/bin/env node
// Render the site in a real browser and assert the things a build cannot.
//
// A Vite build succeeding means the modules resolved. It says nothing about
// whether the wasm engine instantiates, whether the page overflows sideways
// on a phone, whether a locale switch actually changes the text, or whether
// the version in the masthead matches the engine that answered. Each of
// those has a way of being wrong while every other check is green — the
// previous site served a masthead reading 5.0 and a hero reading 4.0 for a
// whole release line, and nothing in CI could see it, because nothing in CI
// opened the page.
//
//   node verify.mjs [url]        default: http://localhost:6040
//   node verify.mjs https://kevy.golia.jp
//
// Needs a Chromium. Resolution order: $CHROME_PATH, the Playwright browser
// cache, then the system Google Chrome.

import { existsSync, readdirSync, readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { chromium } from 'playwright-core'

const HERE = dirname(fileURLToPath(import.meta.url))
const URL_ = process.argv[2] ?? 'http://localhost:6040'

// The version the page must agree with comes from the workspace manifest,
// the same place the build read it from. Asking the page to match itself
// would prove nothing.
function workspaceVersion() {
  const toml = readFileSync(join(HERE, '..', 'Cargo.toml'), 'utf8')
  const m = toml.match(/^version = "(\d+\.\d+\.\d+)"/m)
  if (!m) throw new Error('no workspace version in ../Cargo.toml')
  return m[1]
}

function findBrowser() {
  if (process.env.CHROME_PATH && existsSync(process.env.CHROME_PATH)) {
    return process.env.CHROME_PATH
  }
  const caches = [
    join(homedir(), 'Library/Caches/ms-playwright'),
    join(homedir(), '.cache/ms-playwright'),
  ]
  for (const cache of caches) {
    if (!existsSync(cache)) continue
    for (const dir of readdirSync(cache)) {
      if (!dir.startsWith('chromium')) continue
      for (const rel of [
        'chrome-mac/Chromium.app/Contents/MacOS/Chromium',
        'chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium',
        'chrome-linux/chrome',
      ]) {
        const p = join(cache, dir, rel)
        if (existsSync(p)) return p
      }
    }
  }
  for (const p of [
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
  ]) {
    if (existsSync(p)) return p
  }
  throw new Error('no Chromium found; set CHROME_PATH')
}

const checks = []
function check(name, ok, detail = '') {
  checks.push({ name, ok, detail })
  process.stdout.write(`  ${ok ? '✓' : '✗'} ${name}${detail ? ` — ${detail}` : ''}\n`)
}

const browser = await chromium.launch({ executablePath: findBrowser() })
const errors = []

try {
  const want = workspaceVersion()
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } })
  page.on('pageerror', (e) => errors.push(String(e)))
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text())
  })

  await page.goto(URL_, { waitUntil: 'networkidle' })

  // ── the engine actually runs ────────────────────────────────────────
  // Wait for the terminal to report itself live rather than for a timeout:
  // a fixed sleep passes on a fast machine and flakes on a loaded one, and
  // either way it does not prove the engine started.
  await page.waitForSelector('.term-head .state.live', { timeout: 45_000 })
  check('the wasm engine reaches "live"', true)

  // Typing a command and reading the answer is the only proof the engine
  // is answering rather than merely loaded.
  await page.fill('.term-form input', 'SET verify hello')
  await page.press('.term-form input', 'Enter')
  await page.fill('.term-form input', 'GET verify')
  await page.press('.term-form input', 'Enter')
  await page.waitForFunction(
    () => document.querySelector('.term-body')?.textContent?.includes('"hello"'),
    { timeout: 10_000 },
  )
  check('SET then GET round-trips through the engine', true)

  // An unknown verb must come back as a value, not as an exception that
  // takes the page down.
  await page.fill('.term-form input', 'NOSUCHVERB x')
  await page.press('.term-form input', 'Enter')
  const errText = await page.waitForFunction(
    () => {
      const t = document.querySelector('.term-body')?.textContent ?? ''
      return t.includes('(error)') ? t : null
    },
    { timeout: 10_000 },
  )
  check('an unknown verb answers as data, not a crash', Boolean(await errText.jsonValue()))

  // ── the version is the workspace version ────────────────────────────
  const shown = (await page.textContent('.brand .ver'))?.trim()
  check('masthead version matches the workspace', shown === want, `page=${shown} cargo=${want}`)

  // The wasm build is the embedded store, not the server: it has no INFO
  // and no COMMAND. So the engine-side version is checked where it is
  // actually stated — the resolved package — rather than by asking a verb
  // that does not exist and reading the error as a failure of the page.
  const enginePkg = JSON.parse(
    readFileSync(join(HERE, 'node_modules/@goliapkg/kevy/package.json'), 'utf8'),
  )
  check(
    'the bundled engine package is the workspace version',
    enginePkg.version === want,
    `engine=${enginePkg.version} cargo=${want}`,
  )

  // ── locales ─────────────────────────────────────────────────────────
  // Each switch must change the abstract AND the document language. A
  // switch that sets `lang` without changing the text looks fine in a
  // screenshot and serves English to everyone.
  const abstracts = {}
  for (const [label, code] of [
    ['EN', 'en'],
    ['中文', 'zh-CN'],
    ['日本語', 'ja'],
  ]) {
    await page.click(`.langswitch button:text-is("${label}")`)
    await page.waitForFunction((c) => document.documentElement.lang === c, code)
    abstracts[label] = (await page.textContent('.abstract'))?.slice(0, 60)
  }
  const distinct = new Set(Object.values(abstracts)).size
  check('three locales render three different texts', distinct === 3, `${distinct}/3 distinct`)

  // A missing dictionary key renders as the key itself, which is visible
  // as a bare dotted identifier where prose should be.
  const bare = await page.evaluate(() =>
    [...document.querySelectorAll('.abstract, .lede, .caption, .prose, .card p, .card h3')]
      .map((n) => n.textContent?.trim() ?? '')
      .filter((t) => /^[a-z]+(\.[a-z]+)+$/.test(t)),
  )
  check('no untranslated dictionary keys on the page', bare.length === 0, bare.slice(0, 3).join(' '))

  // ── layout ──────────────────────────────────────────────────────────
  // Grid and flex children default to min-width:auto, so one long unbroken
  // token silently widens the whole document. It is invisible on a desktop
  // and ruins every phone.
  for (const width of [390, 768, 1280]) {
    await page.setViewportSize({ width, height: 900 })
    const over = await page.evaluate(
      () => document.documentElement.scrollWidth > document.documentElement.clientWidth,
    )
    check(`no horizontal overflow at ${width}px`, !over)
  }

  // ── the page did not shout ──────────────────────────────────────────
  // Font CSS from a CDN can fail in a sandbox without breaking anything;
  // that is noise, not a defect this gate owns.
  const real = errors.filter((e) => !/fonts\.(googleapis|gstatic)/.test(e))
  check('no page or console errors', real.length === 0, real.slice(0, 2).join(' | '))
} finally {
  await browser.close()
}

const failed = checks.filter((c) => !c.ok)
process.stdout.write(
  `\n${failed.length === 0 ? 'verify: PASS' : 'verify: FAIL'} — ${
    checks.length - failed.length
  }/${checks.length} checks\n`,
)
process.exit(failed.length === 0 ? 0 : 1)
