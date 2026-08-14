import { renderToStaticMarkup } from 'react-dom/server'

import { Footer } from './components/Footer'
import type { Toc } from './md'

// The reference pages are rendered here, at build time, by the same React
// components and the same stylesheet the landing page uses. They are static
// HTML by the time a reader gets them — 690 pages behind a client-side
// router would be 690 pages a search engine has to execute JavaScript to
// read, for a site whose whole job is being read.
//
// The masthead differs from the landing page's on one point and only one:
// the language switch is links rather than buttons, because a document has
// a translated twin at a known URL and a reader should be able to open it
// in a new tab. The landing page has one URL and switches in place.

export type Lang = 'en' | 'zh' | 'ja'

export type NavItem = { slug: string; title: string }
export type NavGroup = { id: string; label: string; items: NavItem[] }

export type DocPage = {
  lang: Lang
  slug: string
  title: string
  desc: string
  bodyHtml: string
  toc: Toc
  nav: NavGroup[]
  version: string
  /** How many directories deep this page sits, for relative asset links. */
  depth: number
  /** Which other languages have this page, for the switch. */
  have: Lang[]
}

const LANG_LABEL: Record<Lang, string> = { en: 'EN', zh: '中文', ja: '日本語' }
const LANG_HTML: Record<Lang, string> = { en: 'en', zh: 'zh-CN', ja: 'ja' }
const DOCS_LABEL: Record<Lang, string> = { en: 'Docs', zh: '文档', ja: 'ドキュメント' }
const HOME_LABEL: Record<Lang, string> = { en: 'Home', zh: '首页', ja: 'ホーム' }
const ON_THIS_PAGE: Record<Lang, string> = {
  en: 'On this page',
  zh: '本页内容',
  ja: 'このページ',
}

/** `../` repeated, so a page at any depth reaches the site root. */
function up(depth: number) {
  return depth === 0 ? './' : '../'.repeat(depth)
}

/** Where the same document lives in another language. */
function twin(lang: Lang, slug: string, depth: number) {
  const root = up(depth)
  return lang === 'en' ? `${root}docs/${slug}/` : `${root}${lang}/docs/${slug}/`
}

function Doc(p: DocPage) {
  const root = up(p.depth)
  return (
    <>
      <header className="masthead">
        <div className="masthead-inner">
          <a className="brand" href={p.lang === 'en' ? root : `${root}${p.lang}/`}>
            <span className="wordmark">kevy</span>
            <span className="ver">{p.version}</span>
          </a>
          <nav className="topnav">
            <a href={p.lang === 'en' ? root : `${root}${p.lang}/`}>{HOME_LABEL[p.lang]}</a>
            <a href={p.lang === 'en' ? `${root}docs/` : `${root}${p.lang}/docs/`}>
              {DOCS_LABEL[p.lang]}
            </a>
            <div className="langswitch" role="group" aria-label="language">
              {(['en', 'zh', 'ja'] as Lang[]).map((l) =>
                // A language the page does not exist in is not offered. An
                // offer that 404s is worse than no offer.
                p.have.includes(l) ? (
                  <a
                    key={l}
                    className={l === p.lang ? 'on' : ''}
                    href={twin(l, p.slug, p.depth)}
                    hrefLang={LANG_HTML[l]}
                  >
                    {LANG_LABEL[l]}
                  </a>
                ) : null,
              )}
            </div>
          </nav>
        </div>
      </header>

      <div className="docshell">
        <nav className="docnav" aria-label={DOCS_LABEL[p.lang]}>
          {p.nav.map((g) => (
            <div key={g.id}>
              <div className="group">{g.label}</div>
              {g.items.map((it) => (
                <a
                  key={it.slug}
                  className={it.slug === p.slug ? 'on' : ''}
                  href={twin(p.lang, it.slug, p.depth)}
                  aria-current={it.slug === p.slug ? 'page' : undefined}
                >
                  {it.title}
                </a>
              ))}
            </div>
          ))}
        </nav>

        <main className="docmain">
          <div className="breadcrumb">
            <a href={p.lang === 'en' ? root : `${root}${p.lang}/`}>kevy</a>
            {' / '}
            <a href={p.lang === 'en' ? `${root}docs/` : `${root}${p.lang}/docs/`}>
              {DOCS_LABEL[p.lang]}
            </a>
          </div>
          {p.toc.length > 2 && (
            <details className="toc">
              <summary>{ON_THIS_PAGE[p.lang]}</summary>
              <ul>
                {p.toc.map((t) => (
                  <li key={t.slug} className={`l${t.level}`}>
                    <a href={`#${t.slug}`}>{t.text}</a>
                  </li>
                ))}
              </ul>
            </details>
          )}
          {/* The body is HTML produced by src/md.ts from the markdown that
              is this page's canonical source. It is not user input: it comes
              from a file in this repository, through a renderer whose output
              is byte-compared against the previous one on every push. */}
          <div dangerouslySetInnerHTML={{ __html: p.bodyHtml }} />
        </main>
      </div>

      <div className="shell">
        <Footer lang={p.lang} />
      </div>
    </>
  )
}

/** The documentation index — the page /docs/ serves.
 *
 *  It did not exist for the first deploy of this site: every page's nav
 *  linked to /docs/ and every visitor who clicked it got a 404, while
 *  check.mjs reported the link fine because `dist/docs` exists as a
 *  DIRECTORY. A directory is not a page; the gate asks for an index.html
 *  inside one now. */
export type HubPage = {
  lang: Lang
  nav: NavGroup[]
  blurbs: Record<string, string>
  version: string
  depth: number
}

const HUB_TITLE: Record<Lang, string> = {
  en: 'Documentation',
  zh: '文档',
  ja: 'ドキュメント',
}
const HUB_LEDE: Record<Lang, string> = {
  en: 'Every chapter, in reading order. Each one is a markdown file in the repository — what you see here and what GitHub shows are the same text.',
  zh: '全部章节,按阅读顺序排列。每一篇都是仓库里的一个 markdown 文件 —— 你在这里看到的和 GitHub 上显示的是同一份文字。',
  ja: 'すべての章を、読む順に。各章はリポジトリ内の markdown ファイルそのもので、ここで読めるものと GitHub が表示するものは同じ文章です。',
}
const HUB_COMMANDS: Record<Lang, string> = {
  en: 'Command reference',
  zh: '命令参考',
  ja: 'コマンドリファレンス',
}
const HUB_COMMANDS_BLURB: Record<Lang, string> = {
  en: 'Every verb the engine answers, generated from the table it dispatches on.',
  zh: '引擎能回答的每一条动词,由它分发所用的那张表生成。',
  ja: 'エンジンが応答するすべての動詞。ディスパッチに使う表から生成しています。',
}

function Hub(p: HubPage) {
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
            <a href={langRoot(p.lang)}>{p.lang === 'en' ? 'Home' : p.lang === 'zh' ? '首页' : 'ホーム'}</a>
            <div className="langswitch" role="group" aria-label="language">
              {(['en', 'zh', 'ja'] as Lang[]).map((l) => (
                <a
                  key={l}
                  className={l === p.lang ? 'on' : ''}
                  href={`${langRoot(l)}docs/`}
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
        <section className="frontmatter">
          <div className="eyebrow">{HUB_TITLE[p.lang]}</div>
          <h1>{HUB_TITLE[p.lang]}</h1>
          <p className="abstract">{HUB_LEDE[p.lang]}</p>
        </section>

        {p.nav.map((g) => (
          <section key={g.id} id={g.id}>
            <div className="sechead">
              <h2>{g.label}</h2>
            </div>
            <div className="cards">
              {g.items.map((it) => (
                <div className="card" key={it.slug}>
                  <h3>
                    <a href={`${it.slug}/`}>{it.title}</a>
                  </h3>
                  {p.blurbs[it.slug] && <p>{p.blurbs[it.slug]}</p>}
                </div>
              ))}
            </div>
          </section>
        ))}

        <section id="commands">
          <div className="sechead">
            <h2>{HUB_COMMANDS[p.lang]}</h2>
          </div>
          <div className="cards">
            <div className="card">
              <h3>
                <a href="commands/">{HUB_COMMANDS[p.lang]}</a>
              </h3>
              <p>{HUB_COMMANDS_BLURB[p.lang]}</p>
            </div>
          </div>
        </section>

        <Footer lang={p.lang} />
      </div>
    </>
  )
}

export function renderDocHub(p: HubPage, cssHref: string): string {
  const body = renderToStaticMarkup(<Hub {...p} />)
  return `<!doctype html>
<html lang="${LANG_HTML[p.lang]}">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>${HUB_TITLE[p.lang]} · kevy</title>
    <meta name="description" content="${escapeAttr(HUB_LEDE[p.lang])}">
    <link rel="canonical" href="https://kevy.golia.jp${p.lang === 'en' ? '' : `/${p.lang}`}/docs/">
    <meta name="color-scheme" content="light">
    <meta name="theme-color" content="#fcfbf8">
    <link rel="icon" href="${up(p.depth)}kevy-logo.svg" type="image/svg+xml">
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap" rel="stylesheet">
    <link rel="stylesheet" href="${up(p.depth)}${cssHref}">
  </head>
  <body>
${body}
  </body>
</html>
`
}

export function renderDocPage(p: DocPage, cssHref: string): string {
  const body = renderToStaticMarkup(<Doc {...p} />)
  const alt = p.have
    .filter((l) => l !== p.lang)
    .map(
      (l) =>
        `<link rel="alternate" hreflang="${LANG_HTML[l]}" href="https://kevy.golia.jp${
          l === 'en' ? '/docs/' : `/${l}/docs/`
        }${p.slug}/">`,
    )
    .join('\n    ')
  return `<!doctype html>
<html lang="${LANG_HTML[p.lang]}">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>${escapeAttr(p.title)} · kevy</title>
    <meta name="description" content="${escapeAttr(p.desc)}">
    <link rel="canonical" href="https://kevy.golia.jp${
      p.lang === 'en' ? '/docs/' : `/${p.lang}/docs/`
    }${p.slug}/">
    ${alt}
    <meta name="color-scheme" content="light">
    <meta name="theme-color" content="#fcfbf8">
    <link rel="icon" href="${up(p.depth)}kevy-logo.svg" type="image/svg+xml">
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap" rel="stylesheet">
    <link rel="stylesheet" href="${up(p.depth)}${cssHref}">
  </head>
  <body>
${body}
  </body>
</html>
`
}

function escapeAttr(s: string) {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
}
