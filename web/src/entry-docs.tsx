import { type Lang, Layout, page } from './components/Layout'
import type { Toc } from './md'

// The reference pages and their index, rendered at build time through the
// same Layout the landing page uses. Static HTML by the time a reader gets
// them: 690 pages behind a client-side router would be 690 pages a crawler
// has to execute JavaScript to read, for a site whose whole job is being
// read.

export type { Lang }

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
  /** How many directories deep this page sits, for relative links. */
  depth: number
  /** Which other languages have this page. */
  have: Lang[]
}

const ON_THIS_PAGE: Record<Lang, string> = {
  en: 'On this page',
  zh: '本页内容',
  ja: 'このページ',
}
const HUB_TITLE: Record<Lang, string> = { en: 'Documentation', zh: '文档', ja: 'ドキュメント' }
const HUB_LEDE: Record<Lang, string> = {
  en: 'Every chapter, in reading order. Each one is a markdown file in the repository — what you read here and what GitHub shows are the same text.',
  zh: '全部章节，按阅读顺序排列。每一篇都是仓库里的一个 markdown 文件 —— 你在这里读到的和 GitHub 上显示的是同一份文字。',
  ja: 'すべての章を、読む順に。各章はリポジトリ内の markdown ファイルそのもので、ここで読めるものと GitHub が表示するものは同じ文章です。',
}
const HUB_COMMANDS: Record<Lang, string> = {
  en: 'Command reference',
  zh: '命令参考',
  ja: 'コマンドリファレンス',
}
const HUB_COMMANDS_BLURB: Record<Lang, string> = {
  en: 'Every verb the engine answers, generated from the table it dispatches on.',
  zh: '引擎能回答的每一条动词，由它分发所用的那张表生成。',
  ja: 'エンジンが応答するすべての動詞。ディスパッチに使う表から生成しています。',
}

const up = (depth: number) => (depth === 0 ? './' : '../'.repeat(depth))

/** Where the same document lives in another language. */
function twin(lang: Lang, slug: string, depth: number) {
  const root = up(depth)
  return lang === 'en' ? `${root}docs/${slug}/` : `${root}${lang}/docs/${slug}/`
}

function Sidebar({
  nav,
  lang,
  slug,
  depth,
}: {
  nav: NavGroup[]
  lang: Lang
  slug: string
  depth: number
}) {
  return (
    <nav className="docnav" aria-label="documentation">
      {nav.map((g) => (
        <div key={g.id}>
          <div className="group">{g.label}</div>
          {g.items.map((it) => (
            <a
              key={it.slug}
              className={it.slug === slug ? 'on' : undefined}
              href={twin(lang, it.slug, depth)}
              aria-current={it.slug === slug ? 'page' : undefined}
            >
              {it.title}
            </a>
          ))}
        </div>
      ))}
    </nav>
  )
}

export function renderDocPage(p: DocPage, css: string): string {
  const root = up(p.depth)
  return page(
    {
      lang: p.lang,
      version: p.version,
      title: `${p.title} · kevy`,
      desc: p.desc,
      canonical: `${p.lang === 'en' ? '' : `/${p.lang}`}/docs/${p.slug}/`,
      root,
      css,
      alternates: p.have
        .filter((l) => l !== p.lang)
        .map((l) => ({ lang: l, path: `${l === 'en' ? '' : `/${l}`}/docs/${p.slug}/` })),
    },
    <Layout
      lang={p.lang}
      root={root}
      here="docs"
      langs={{ kind: 'links', href: (l) => twin(l, p.slug, p.depth), have: p.have }}
      aside={<Sidebar nav={p.nav} lang={p.lang} slug={p.slug} depth={p.depth} />}
    >
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
      {/* HTML produced by src/md.ts from this page's canonical markdown. Not
          user input: a file in this repository, through a renderer whose
          output is byte-compared against the previous one on every push. */}
      <div dangerouslySetInnerHTML={{ __html: p.bodyHtml }} />
    </Layout>,
  )
}

export type HubPage = {
  lang: Lang
  nav: NavGroup[]
  blurbs: Record<string, string>
  version: string
  depth: number
}

export function renderDocHub(p: HubPage, css: string): string {
  const root = up(p.depth)
  const langRoot = (l: Lang) => (l === 'en' ? root : `${root}${l}/`)
  return page(
    {
      lang: p.lang,
      version: p.version,
      title: `${HUB_TITLE[p.lang]} · kevy`,
      desc: HUB_LEDE[p.lang],
      canonical: `${p.lang === 'en' ? '' : `/${p.lang}`}/docs/`,
      root,
      css,
    },
    <Layout
      lang={p.lang}
      root={root}
      here="docs"
      langs={{ kind: 'links', href: (l) => `${langRoot(l)}docs/` }}
    >
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
    </Layout>,
  )
}
