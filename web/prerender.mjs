#!/usr/bin/env node
// Render every reference page to static HTML, at build time.
//
// The markdown under docs/ is the canonical source — it is what GitHub
// shows and what translators edit. This turns it into pages using the same
// renderer (src/md.ts) and the same React components as the landing page,
// so the whole site is one system rather than two that resemble each other.
//
// Static output, not a client-side router: a documentation site whose job
// is being read should not require JavaScript to be read, and 690 routes
// behind a router is 690 pages a crawler has to execute to see.
//
// Run through `npm run build`, which builds the SSR bundle first.

import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const ROOT = join(HERE, '..')
const DIST = join(HERE, 'dist')
const DOCS = join(ROOT, 'docs')

const { renderDocPage } = await import('./.ssr/entry-docs.js')
const { render } = await import('./.ssr/md.js')

// ── the version, from the one place that has it ──────────────────────────
const VERSION = readFileSync(join(ROOT, 'Cargo.toml'), 'utf8').match(
  /^version = "(\d+\.\d+\.\d+)"/m,
)?.[1]
if (!VERSION) throw new Error('no workspace version in Cargo.toml')

// ── the nav, mirrored from the sections the docs are organised into ──────
// Kept here rather than derived from the filesystem: the order is editorial
// (what a reader should meet first), and a directory listing is alphabetical.
const SECTIONS = [
  ['start', { en: 'Getting started', zh: '上手', ja: 'はじめに' },
    ['designing-on-kevy', 'cookbook', 'persistence', 'tuning']],
  ['deploy', { en: 'Running it', zh: '运行', ja: '運用' },
    ['upgrading-5.0-to-5.1', 'upgrading-4-to-5', 'replication', 'availability', 'cluster',
     'tiering', 'accept-shards', 'alloc', 'uds', 'async']],
  ['data', { en: 'Working with data', zh: '数据', ja: 'データ' },
    ['indexes', 'tables', 'table-migration', 'vector-search', 'text-search', 'views',
     'cdc', 'pubsub', 'lua']],
  ['embed', { en: 'Embedding it', zh: '嵌入', ja: '組み込み' },
    ['wasm', 'embedded-listener', 'electron', 'tauri', 'iot']],
  ['clients', { en: 'Clients', zh: '客户端', ja: 'クライアント' },
    ['clients', 'bindings', 'client-contract']],
  ['ref', { en: 'Reference', zh: '参考', ja: 'リファレンス' },
    ['boundaries', 'error-replies', 'rds-workloads', 'deploy-behind-a-proxy',
     'migration', 'UPGRADING']],
]

const LANGS = ['en', 'zh', 'ja']

// Engineering correspondence, and one page the command reference replaced.
// They live in the repository for the people who go looking; putting dated
// defect reports in a nav would be filing them as documentation.
const EXCLUDE = new Set([
  'verb-reference',
  'DEFECT-REPORT-2026-07-20-ATOMIC-ERROR-PATH-RESPONSE',
  'REPORT-FROM-GOLIAJP-2026-07-20-EMBEDDED-AS-PRIMARY-STORE',
  'REPORT-RESPONSE-2026-07-20-EMBEDDED-AS-PRIMARY-STORE',
  'SUPPORT-LINE-3X-VS-4X-2026-07-20',
])

function docDir(lang) {
  return lang === 'en' ? DOCS : join(DOCS, lang)
}

function slugsFor(lang) {
  const dir = docDir(lang)
  if (!existsSync(dir)) return []
  return readdirSync(dir)
    .filter((f) => f.endsWith('.md'))
    .map((f) => f.slice(0, -3))
    .filter((s) => !EXCLUDE.has(s))
}

/** First `# ` heading, or the slug — a page with no title is a page nobody
 *  can find in a nav. */
function titleOf(md, slug) {
  const m = md.match(/^#\s+(.+)$/m)
  return m ? m[1].replace(/`/g, '').trim() : slug
}

/** First paragraph, trimmed to a meta description length. */
function descOf(md) {
  const body = md.replace(/^#\s+.+$/m, '').trim()
  const para = body.split('\n\n').find((p) => p.trim() && !p.startsWith('#') && !p.startsWith('>'))
  if (!para) return 'kevy — a Redis-compatible engine that goes further.'
  return para.replace(/[*`_>|]/g, '').replace(/\s+/g, ' ').trim().slice(0, 180)
}

// Links between markdown files must become links between pages. `foo.md`
// and `foo.md#bar` are siblings; anything else is left alone.
// The repository on GitHub, for the files that have no page here. A link
// to a benchmark ledger should go somewhere, and the somewhere is the file
// itself — dropping it leaves a reader with a name and no way to reach it.
const GH = 'https://github.com/goliajp/kevy/blob/develop/'

function linkMapFor(lang, present) {
  return function linkMap(href) {
  if (/^(https?:|mailto:|#|\/)/.test(href)) return href

  // A sibling document. `./foo.md`, `foo.md` and `../foo.md` all mean the
  // same page: docs are flat, and the `../` form appears because in the
  // markdown tree a translation sits one level deeper. Handling only the
  // bare form left 13 dead links per language.
  const sibling = /^(?:\.{1,2}\/)*([\w.-]+)\.md(#.*)?$/.exec(href)
  if (sibling) {
    const slug = sibling[1]
    const frag = sibling[2] ?? ''
    // A page the site does not publish is not a page to link to. Send the
    // reader to the file in the repository, which is where it lives.
    if (EXCLUDE.has(slug)) return `${GH}docs/${slug}.md${frag}`
    // A translation that links to a page its language does not have would
    // 404. The English original is the honest target: the reader gets the
    // document, in the language it exists in, rather than nothing.
    if (!present.has(slug)) {
      const root = lang === 'en' ? '../../' : '../../../'
      return `${root}docs/${slug}/${frag}`
    }
    return `../${slug}/${frag}`
  }

  // A path into the repository — a benchmark report, a script, a source
  // file. None of these has a page; all of them exist on GitHub.
  if (/^(?:\.{1,2}\/)*(bench|crates|tools|packaging|scripts|\.github)\//.test(href)) {
    return GH + href.replace(/^(?:\.{1,2}\/)+/, '')
  }
  if (/\.(rs|py|sh|toml|json|ya?ml|txt)$/.test(href)) {
    return GH + href.replace(/^(?:\.{1,2}\/)+/, '')
  }
  return href
  }
}

// ── the stylesheet Vite emitted, by name ─────────────────────────────────
// Hashed, so it cannot be hardcoded; read from the built index.html, which
// is the only place that knows.
function cssHref() {
  const html = readFileSync(join(DIST, 'index.html'), 'utf8')
  const m = html.match(/href="\/(assets\/[^"]+\.css)"/)
  if (!m) throw new Error('no stylesheet in dist/index.html — did vite build run?')
  return m[1]
}

const CSS = cssHref()
let written = 0

for (const lang of LANGS) {
  const slugs = slugsFor(lang)
  if (slugs.length === 0) continue
  const present = new Set(slugs)

  // Titles first: the nav needs every page's title, so it cannot be built
  // while rendering the pages one at a time.
  const titles = {}
  const sources = {}
  for (const slug of slugs) {
    const md = readFileSync(join(docDir(lang), `${slug}.md`), 'utf8')
    sources[slug] = md
    titles[slug] = titleOf(md, slug)
  }

  const nav = SECTIONS.map(([id, label, items]) => ({
    id,
    label: label[lang],
    items: items.filter((s) => present.has(s)).map((s) => ({ slug: s, title: titles[s] })),
  })).filter((g) => g.items.length > 0)

  // A page in the sections but absent from disk, or on disk but in no
  // section, is a page a reader cannot navigate to. Say so rather than
  // quietly leaving it orphaned.
  const inNav = new Set(SECTIONS.flatMap(([, , items]) => items))
  const orphans = slugs.filter((s) => !inNav.has(s))
  if (orphans.length) {
    process.stderr.write(
      `prerender: ${lang}: these pages are in no nav section, so nothing links to them:\n` +
        orphans.map((o) => `  ${o}\n`).join('') +
        `Add them to SECTIONS, or to EXCLUDE if they should not be on the site.\n`,
    )
    process.exitCode = 1
  }

  for (const slug of slugs) {
    const md = sources[slug]
    const { html, toc } = render(md, linkMapFor(lang, present))
    const have = LANGS.filter((l) => existsSync(join(docDir(l), `${slug}.md`)))
    const outDir =
      lang === 'en' ? join(DIST, 'docs', slug) : join(DIST, lang, 'docs', slug)
    mkdirSync(outDir, { recursive: true })
    writeFileSync(
      join(outDir, 'index.html'),
      renderDocPage(
        {
          lang,
          slug,
          title: titles[slug],
          desc: descOf(md),
          bodyHtml: html,
          toc,
          nav,
          version: VERSION,
          depth: lang === 'en' ? 2 : 3,
          have,
        },
        CSS,
      ),
    )
    written++
  }
}

// ── the command reference ────────────────────────────────────────────────
// One page per verb, in each language, from the table the engine
// dispatches on. Nothing here is written by hand, which is the point: a
// verb that gains a flag changes these pages by changing the code.
const { renderCommandIndex, renderCommandPage } = await import('./.ssr/entry-commands.js')
const commands = JSON.parse(
  readFileSync(join(HERE, 'src/commands.json'), 'utf8'),
).commands
if (!Array.isArray(commands) || commands.length === 0) {
  throw new Error('site/data/commands.json holds no commands')
}

let cmdPages = 0
for (const lang of LANGS) {
  const base = lang === 'en' ? join(DIST, 'docs', 'commands') : join(DIST, lang, 'docs', 'commands')
  const depth = lang === 'en' ? 2 : 3
  mkdirSync(base, { recursive: true })
  writeFileSync(join(base, 'index.html'), renderCommandIndex(lang, commands, VERSION, depth, CSS))
  cmdPages++
  for (const c of commands) {
    // A verb name can carry a dot (IDX.CREATE) or a bar; the directory
    // name has to survive a filesystem and a URL both.
    const slug = c.name.toLowerCase().replace(/[^a-z0-9.]/g, '-')
    const dir = join(base, slug)
    mkdirSync(dir, { recursive: true })
    writeFileSync(join(dir, 'index.html'), renderCommandPage(lang, c, VERSION, depth + 1, CSS))
    cmdPages++
  }
}

// ── the written pages ────────────────────────────────────────────────────
// The scenario guides, the migration guide, the benchmark report and the
// capacity calculator, from content exported verbatim out of the Python
// dicts that have held it since the previous site.
const { renderPage } = await import('./.ssr/entry-pages.js')
const CONTENT = JSON.parse(readFileSync(join(HERE, 'src/content.json'), 'utf8'))

let writtenPages = 0
for (const lang of LANGS) {
  const pages = CONTENT[lang]
  if (!pages) throw new Error(`content.json has no ${lang}`)
  for (const [slug, page] of Object.entries(pages)) {
    // The home page is the React landing page, which Vite already built.
    // The content module still carries its blocks because the two shared a
    // source; rendering them again would overwrite index.html with a
    // second, static home page.
    if (slug === '') continue
    const dir = lang === 'en' ? join(DIST, slug) : join(DIST, lang, slug)
    mkdirSync(dir, { recursive: true })
    const depth = (lang === 'en' ? 0 : 1) + slug.split('/').length
    const hasCalc = page.blocks.some((b) => (b.t ?? b.kind) === 'calc')
    writeFileSync(
      join(dir, 'index.html'),
      renderPage(
        { lang, slug, title: page.title, desc: page.desc, blocks: page.blocks, version: VERSION, depth },
        CSS,
        hasCalc,
      ),
    )
    writtenPages++
  }
  // Each language needs its own landing page too. English is served by the
  // React build at the root; zh and ja get a static one from the same
  // content, so a reader who lands on /zh/ is not sent to English.
  if (lang !== 'en') {
    const home = pages['']
    if (home) {
      mkdirSync(join(DIST, lang), { recursive: true })
      writeFileSync(
        join(DIST, lang, 'index.html'),
        renderPage(
          { lang, slug: '', title: home.title, desc: home.desc, blocks: home.blocks, version: VERSION, depth: 1 },
          CSS,
          false,
        ),
      )
      writtenPages++
    }
  }
}

// ── llms.txt ─────────────────────────────────────────────────────────────
// A plain-text index of the site for language models, and the full text
// behind it. Generated here rather than written, so it cannot describe a
// site that no longer exists.
{
  const lines = [
    `# kevy ${VERSION}`,
    '',
    '> A Redis-compatible engine with vector search, full text, secondary',
    '> indexes, materialised views and a change feed inside it. Pure Rust,',
    '> no third-party crates.',
    '',
    '## Documentation',
    '',
  ]
  const full = [...lines]
  for (const lang of LANGS) {
    for (const slug of slugsFor(lang)) {
      const url = `https://kevy.golia.jp${lang === 'en' ? '' : `/${lang}`}/docs/${slug}/`
      if (lang === 'en') lines.push(`- [${slug}](${url})`)
      full.push(`\n\n# ${slug} (${lang})\n${url}\n`)
      full.push(readFileSync(join(docDir(lang), `${slug}.md`), 'utf8'))
    }
  }
  writeFileSync(join(DIST, 'llms.txt'), lines.join('\n') + '\n')
  writeFileSync(join(DIST, 'llms-full.txt'), full.join('\n') + '\n')
}

process.stdout.write(
  `prerender: ${written} reference, ${cmdPages} command, ${writtenPages} written pages, llms.txt\n`,
)

// The engine binary the landing page loads is also what the docs describe.
// Nothing here depends on it, but a build that quietly shipped a different
// version than the manifest says would be the exact failure this whole
// pipeline exists to prevent — so it is asserted, once, cheaply.
const enginePkg = JSON.parse(
  readFileSync(join(HERE, 'node_modules/@goliapkg/kevy/package.json'), 'utf8'),
)
if (enginePkg.version !== VERSION) {
  process.stderr.write(
    `prerender: the bundled engine is ${enginePkg.version}, the workspace is ${VERSION}\n`,
  )
  process.exit(1)
}

// Git's idea of the tree, for the one thing a static build cannot know:
// whether these pages were generated from the sources currently committed.
// check.mjs re-runs the build and diffs; this just records what it saw.
try {
  const rev = execFileSync('git', ['-C', ROOT, 'rev-parse', '--short', 'HEAD'], {
    encoding: 'utf8',
  }).trim()
  writeFileSync(join(DIST, 'build.json'), JSON.stringify({ version: VERSION, rev }) + '\n')
} catch {
  // Not a git checkout — a tarball build is legitimate, and the stamp is
  // informational.
}
