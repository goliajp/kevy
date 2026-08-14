import { Block } from './components/Blocks'
import { type Lang, Layout, page } from './components/Layout'

// The written pages — the scenario guides, the migration guide, the
// benchmark report, the capacity calculator — through the same Layout as
// everything else, from content exported out of
// tools/site_content/{en,zh,ja}.py.
//
// The content is not rewritten here. It is 4,600 lines of prose written
// and translated once; a hand migration is a migration that loses some of
// it, and check_site_content_parity.py compares what comes out against
// what went in so that cannot happen quietly.

export type { Lang }

const up = (depth: number) => (depth === 0 ? './' : '../'.repeat(depth))

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
 *  translations without every string knowing how deep it sits. Resolved
 *  here rather than in the content, which is why the content survived the
 *  move unedited. */
function resolveLinks(html: string, lang: Lang, depth: number): string {
  const root = up(depth)
  const langRoot = lang === 'en' ? root : `${root}${lang}/`
  // llms.txt is one file served from the site root in every language, not
  // a translated page.
  html = html.replace(/(href|src)="~\/llms/g, `$1="${root}llms`)
  // The playground is not a page: it is the terminal on the landing page,
  // in the same shell as everything else.
  html = html.replace(/(href|src)="~\/play\/"/g, `$1="${langRoot}#try"`)
  html = html.replace(/(href|src)="~\//g, `$1="${langRoot}`)
  // Card and tab links are written root-relative in the content. Without
  // an equivalent of the previous renderer's loc() helper they resolve
  // against whatever directory the page sits in — on use/cache/ that made
  // `docs/persistence/` mean `use/cache/docs/persistence/`.
  return html.replace(
    /(href)="(docs\/|benchmarks\/|capacity\/|choose\/|migrate\/|use\/)/g,
    `$1="${langRoot}$2`,
  )
}

// The tiering ceiling, from docs/tiering.md: max data:RAM is the value size
// over the per-key overhead (96 B, plus key heap for keys longer than the
// 22 B inline limit). Below 64 B a value's stub is as big as the value, so
// nothing tiers and the ceiling is 1x — the page says so rather than
// quietly extrapolating past where the model was measured.
const CALC_JS = `    (function () {
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
    })()`

export function renderPage(p: PageInput, css: string, calcJs: boolean): string {
  const root = up(p.depth)
  const langRoot = (l: Lang) => (l === 'en' ? root : `${root}${l}/`)
  const html = page(
    {
      lang: p.lang,
      title: p.title,
      desc: p.desc,
      canonical: `${p.lang === 'en' ? '' : `/${p.lang}`}/${p.slug ? `${p.slug}/` : ''}`,
      root,
      css,
      script: calcJs ? CALC_JS : undefined,
    },
    <Layout
      lang={p.lang}
      version={p.version}
      root={root}
      langs={{ kind: 'links', href: (l) => `${langRoot(l)}${p.slug ? `${p.slug}/` : ''}` }}
    >
      {p.blocks.map((b, i) => (
        <Block key={i} b={b as never} version={p.version} uid={`t${i}`} />
      ))}
    </Layout>,
  )
  return resolveLinks(html, p.lang, p.depth)
}
