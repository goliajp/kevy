// Where a Chromium is, for everything here that needs one.
//
// Resolution order: $CHROME_PATH, a Playwright-downloaded browser in the
// per-user cache, then the system install.
//
// This is a module, and run directly it prints the path (or exits 1),
// because two different languages need the same answer. `verify.mjs`
// imports it to launch a browser; `tools/suite.py` runs it to decide
// whether sitegate can run at all. That probe used to ask a different
// question — whether `web/node_modules/playwright-core` existed — and
// playwright installs its browsers separately from itself, so a box with
// the library and no browser reported the requirement satisfied and then
// failed sitegate inside `chromium.launch`, with a stack trace that says
// nothing about the site.
import { existsSync, readdirSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

export function findBrowser() {
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

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    process.stdout.write(findBrowser() + '\n')
  } catch (e) {
    process.stderr.write(String(e.message ?? e) + '\n')
    process.exit(1)
  }
}
