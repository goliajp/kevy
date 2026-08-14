// The two Golia Lab pages are one publication, so the shell they share —
// masthead and footer — should be the same object rendered twice, not two
// objects that look alike. This compares them property by property against
// the live tiktoken.golia.jp, which is the only way to answer "are they the
// same?" without doing it by eye. Three rounds of eyeballing found the
// wordmark and missed the link underline, the licence wording and the
// duplicated organisation name.
//
// Deliberately not in CI: it fails when the *other* site changes, which is
// not something this repository can act on at that moment — the same reason
// check_action_versions.py keeps its --online mode out of the gate. Run it
// when the shell changes:
//
//   cd web && npm run build && npx serve dist &   # or any static server
//   node compare-lab.mjs http://localhost:6041
import { chromium } from 'playwright-core'
import { existsSync } from 'node:fs'

const MINE = process.argv[2] || 'http://localhost:6041'
const THEIRS = 'https://tiktoken.golia.jp'

// Height is excluded on purpose: the two sites have different nav labels in
// different languages, so it differs by a fraction of a pixel for a reason
// that is not drift.
const HEADER = ['position','borderBottomWidth','borderBottomStyle','borderBottomColor',
  'paddingTop','paddingBottom','backgroundColor','display','alignItems','gap','fontSize']
const FOOTER = ['borderTopWidth','borderTopStyle','paddingTop','paddingBottom','fontSize',
  'color','display','gridTemplateColumns','gap']
const LINK = ['color','borderBottomWidth','borderBottomStyle','borderBottomColor',
  'display','gap','alignItems','fontSize']

const chrome = [
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/usr/bin/google-chrome',
].find(existsSync)
const browser = await chromium.launch(chrome ? { executablePath: chrome } : {})

async function grab(url) {
  const page = await browser.newPage()
  await page.setViewportSize({ width: 1200, height: 900 })
  await page.goto(url, { waitUntil: 'networkidle' })
  const shot = await page.evaluate(
    ([H, F, L]) => {
      const pick = (el, keys) =>
        Object.fromEntries(keys.map((k) => [k, getComputedStyle(el)[k]]))
      const head = document.querySelector('header.masthead')
      const foot = document.querySelector('footer')
      const texts = [...foot.querySelectorAll('div')]
        .map((d) => d.textContent.trim())
        .filter((t) => t && !/GitHub/.test(t))
      return {
        header: pick(head, H),
        footer: pick(foot, F),
        link: pick(foot.querySelector('.links a'), L),
        licence: texts[texts.length - 1],
        links: [...foot.querySelectorAll('.links a')].map((a) => a.textContent.trim()),
      }
    },
    [HEADER, FOOTER, LINK],
  )
  await page.close()
  return shot
}

const [mine, theirs] = await Promise.all([grab(MINE), grab(THEIRS)])
await browser.close()

let bad = 0
for (const section of ['header', 'footer', 'link']) {
  for (const k of Object.keys(theirs[section])) {
    if (mine[section][k] !== theirs[section][k]) {
      console.log(`  ✗ ${section}.${k}\n      kevy     ${mine[section][k]}\n      tiktoken ${theirs[section][k]}`)
      bad++
    }
  }
}
if (mine.licence !== theirs.licence) {
  console.log(`  ✗ licence line\n      kevy     ${mine.licence}\n      tiktoken ${theirs.licence}`)
  bad++
}
console.log(
  bad
    ? `compare-lab: ${bad} difference(s) between the two lab pages`
    : `compare-lab: identical — ${HEADER.length + FOOTER.length + LINK.length} properties and the licence line`,
)
process.exit(bad ? 1 : 0)
