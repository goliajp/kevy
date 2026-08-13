import { renderToStaticMarkup } from 'react-dom/server'

import { Block } from './components/Blocks'
import { Footer } from './components/Footer'

// The eleven written pages — the scenario guides, the migration guide, the
// benchmark report, the capacity calculator — rendered at build time from
// the content exported out of tools/site_content/{en,zh,ja}.py.
//
// The content is not rewritten here. It is 4,600 lines of prose that was
// written and translated once; a hand migration is a migration that loses
// some of it, and check_site_content_parity.py compares what comes out
// against what went in so that cannot happen quietly.

export type Lang = 'en' | 'zh' | 'ja'

const LANG_HTML: Record<Lang, string> = { en: 'en', zh: 'zh-CN', ja: 'ja' }
const LANG_LABEL: Record<Lang, string> = { en: 'EN', zh: '中文', ja: '日本語' }
const DOCS: Record<Lang, string> = { en: 'Docs', zh: '文档', ja: 'ドキュメント' }
const HOME: Record<Lang, string> = { en: 'Home', zh: '首页', ja: 'ホーム' }

function up(depth: number) {
  return depth === 0 ? './' : '../'.repeat(depth)
}

function esc(s: string) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;')
}

export type PageInput = {
  lang: Lang
  /** '' for the home page, otherwise 'use/cache' and friends. */
  slug: string
  title: string
  desc: string
  blocks: unknown[]
  version: string
  depth: number
}

/** `~/` in the content means "this language's root" — the convention the
 *  previous site used so a translated page could link to its own
 *  translations without every string knowing how deep it sits. It is
 *  resolved here rather than in the content, which is why the content
 *  survived the move unedited. */
function resolveTildes(html: string, lang: Lang, depth: number): string {
  const root = depth === 0 ? './' : '../'.repeat(depth)
  const langRoot = lang === 'en' ? root : `${root}${lang}/`
  // llms.txt is served from the site root in every language: it is one
  // file, not a translated page.
  html = html.replace(/(href|src)="~\/llms/g, `$1="${root}llms`)
  // The playground is not a page any more. It is the terminal on the
  // landing page, in the same shell as everything else — so a link to it
  // is a link to that section, not to a surface that no longer exists.
  html = html.replace(/(href|src)="~\/play\/"/g, `$1="${langRoot}#try"`)
  return html.replace(/(href|src)="~\//g, `$1="${langRoot}`)
}

function Page(p: PageInput) {
  const root = up(p.depth)
  const langRoot = (l: Lang) => (l === 'en' ? root : `${root}${l}/`)
  return (
    <>
      <header className="masthead">
        <div className="masthead-inner">
          <a className="brand" href={langRoot(p.lang)}>
            <span className="wordmark">kevy</span>
            <span className="ver">{p.version}</span>
          </a>
          <nav className="topnav">
            <a href={langRoot(p.lang)}>{HOME[p.lang]}</a>
            <a href={`${langRoot(p.lang)}docs/`}>{DOCS[p.lang]}</a>
            <div className="langswitch" role="group" aria-label="language">
              {(['en', 'zh', 'ja'] as Lang[]).map((l) => (
                <a
                  key={l}
                  className={l === p.lang ? 'on' : ''}
                  href={`${langRoot(l)}${p.slug ? `${p.slug}/` : ''}`}
                  hrefLang={LANG_HTML[l]}
                >
                  {LANG_LABEL[l]}
                </a>
              ))}
            </div>
          </nav>
        </div>
      </header>
      <div className="shell">
        {p.blocks.map((b, i) => (
          <Block key={i} b={b as never} version={p.version} uid={`t${i}`} />
        ))}
        <Footer lang={p.lang} />
      </div>
    </>
  )
}

export function renderPage(p: PageInput, css: string, calcJs: boolean): string {
  const body = resolveTildes(renderToStaticMarkup(<Page {...p} />), p.lang, p.depth)
  const canonical = `https://kevy.golia.jp${p.lang === 'en' ? '' : `/${p.lang}`}/${
    p.slug ? `${p.slug}/` : ''
  }`
  return `<!doctype html>
<html lang="${LANG_HTML[p.lang]}">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>${esc(p.title)}</title>
    <meta name="description" content="${esc(p.desc)}">
    <link rel="canonical" href="${canonical}">
    <meta name="color-scheme" content="light">
    <meta name="theme-color" content="#fcfbf8">
    <link rel="icon" href="${up(p.depth)}kevy-logo.svg" type="image/svg+xml">
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap" rel="stylesheet">
    <link rel="stylesheet" href="${up(p.depth)}${css}">
  </head>
  <body>
${body}${
    calcJs
      ? `
    <script>
    // The tiering ceiling, from docs/tiering.md: max data:RAM is the value
    // size over the per-key overhead (96 B, plus key heap for keys longer
    // than the 22 B inline limit). Below 64 B a value's stub is as big as
    // the value, so nothing tiers and the ceiling is 1x — the page says so
    // rather than quietly extrapolating a curve past where it was measured.
    (function () {
      var form = document.querySelector('.calc')
      if (!form) return
      var out = form.querySelector('[data-calc-out]')
      function num(name, dflt) {
        var el = form.querySelector('input[data-calc="' + name + '"]')
        var v = el ? parseFloat(el.value) : NaN
        return isFinite(v) && v > 0 ? v : dflt
      }
      function gib(bytes) {
        var u = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'], i = 0
        while (bytes >= 1024 && i < u.length - 1) { bytes /= 1024; i++ }
        return bytes.toFixed(bytes < 10 ? 2 : 0) + ' ' + u[i]
      }
      function recompute() {
        var value = num('value', 4096), key = num('key', 24), budget = num('budget', 32)
        if (value < 64) { out.textContent = out.dataset.lBelow; return }
        var keyHeap = key <= 22 ? 0 : key
        var ratio = value / (96 + keyHeap)
        var served = budget * 1024 * 1024 * 1024 * ratio
        out.textContent =
          ratio.toFixed(1) + 'x ' + out.dataset.lRatio + ' — ' +
          gib(served) + ' ' + out.dataset.lServed
      }
      form.addEventListener('input', recompute)
      recompute()
    })()
    </script>`
      : ''
  }
  </body>
</html>
`
}
