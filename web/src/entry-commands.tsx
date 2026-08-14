import { renderToStaticMarkup } from 'react-dom/server'

import { Footer } from './components/Footer'

// The command reference: one page per verb, plus an index. Generated from
// site/data/commands.json, which is itself generated from VERB_META in the
// engine — so a verb that gains a flag or loses an argument changes these
// pages by changing the code, and there is no second description to update.

export type Lang = 'en' | 'zh' | 'ja'

export type Command = {
  name: string
  group: string
  arity: number
  flags: string[]
  since: string
  syntax: string
  summary: string
  complexity?: string
  compat?: string
}

const L = {
  en: {
    docs: 'Docs',
    home: 'Home',
    commands: 'Commands',
    index: 'Command reference',
    lede: (n: number) =>
      `Every verb the engine answers — ${n} of them, generated from the same table the server dispatches on.`,
    syntax: 'Syntax',
    since: 'Since',
    complexity: 'Complexity',
    compat: 'Redis compatibility',
    flags: 'Flags',
    group: 'Group',
    arity: 'Arity',
    all: 'All commands',
  },
  zh: {
    docs: '文档',
    home: '首页',
    commands: '命令',
    index: '命令参考',
    lede: (n: number) => `引擎能回答的每一条动词 —— 共 ${n} 条，与服务端分发所用的是同一张表。`,
    syntax: '语法',
    since: '起始版本',
    complexity: '复杂度',
    compat: 'Redis 兼容性',
    flags: '标志',
    group: '分组',
    arity: '参数个数',
    all: '全部命令',
  },
  ja: {
    docs: 'ドキュメント',
    home: 'ホーム',
    commands: 'コマンド',
    index: 'コマンドリファレンス',
    lede: (n: number) =>
      `エンジンが応答するすべての動詞——全 ${n} 件。サーバーがディスパッチに使う表から生成しています。`,
    syntax: '構文',
    since: '導入バージョン',
    complexity: '計算量',
    compat: 'Redis 互換性',
    flags: 'フラグ',
    group: 'グループ',
    arity: '引数の数',
    all: 'すべてのコマンド',
  },
} as const

const LANG_HTML: Record<Lang, string> = { en: 'en', zh: 'zh-CN', ja: 'ja' }
const LANG_LABEL: Record<Lang, string> = { en: 'EN', zh: '中文', ja: '日本語' }

function up(depth: number) {
  return '../'.repeat(depth)
}

function Chrome({
  lang,
  depth,
  version,
  here,
  children,
}: {
  lang: Lang
  depth: number
  version: string
  /** `''` for the index, otherwise the verb name. */
  here: string
  children: React.ReactNode
}) {
  const root = up(depth)
  const langRoot = (l: Lang) => (l === 'en' ? root : `${root}${l}/`)
  return (
    <>
      <header className="masthead">
        <div className="masthead-inner">
          <a className="brand" href={langRoot(lang)}>
            <span className="wordmark">kevy</span>
            <span className="ver">{version}</span>
          </a>
          <nav className="topnav">
            <a href={langRoot(lang)}>{L[lang].home}</a>
            <a href={`${langRoot(lang)}docs/`}>{L[lang].docs}</a>
            <div className="langswitch" role="group" aria-label="language">
              {(['en', 'zh', 'ja'] as Lang[]).map((l) => (
                <a
                  key={l}
                  className={l === lang ? 'on' : ''}
                  href={`${langRoot(l)}docs/commands/${here ? `${here.toLowerCase()}/` : ''}`}
                  hrefLang={LANG_HTML[l]}
                >
                  {LANG_LABEL[l]}
                </a>
              ))}
            </div>
          </nav>
        </div>
      </header>
      <div className="shell">{children}</div>
      <div className="shell">
        <Footer lang={lang} />
      </div>
    </>
  )
}

function esc(s: string) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;')
}

function doc(lang: Lang, title: string, desc: string, canonical: string, depth: number, css: string, body: string) {
  return `<!doctype html>
<html lang="${LANG_HTML[lang]}">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>${esc(title)} · kevy</title>
    <meta name="description" content="${esc(desc)}">
    <link rel="canonical" href="https://kevy.golia.jp${canonical}">
    <meta name="color-scheme" content="light">
    <meta name="theme-color" content="#fcfbf8">
    <link rel="icon" href="${up(depth)}kevy-logo.svg" type="image/svg+xml">
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap" rel="stylesheet">
    <link rel="stylesheet" href="${up(depth)}${css}">
  </head>
  <body>
${body}
  </body>
</html>
`
}

export function renderCommandIndex(
  lang: Lang,
  cmds: Command[],
  version: string,
  depth: number,
  css: string,
): string {
  const t = L[lang]
  // Grouped, because 191 verbs in one alphabetical run is a list nobody
  // reads. The groups come from the engine's own table.
  const groups = new Map<string, Command[]>()
  for (const c of cmds) {
    if (!groups.has(c.group)) groups.set(c.group, [])
    groups.get(c.group)!.push(c)
  }
  const body = renderToStaticMarkup(
    <Chrome lang={lang} depth={depth} version={version} here="">
      <section className="frontmatter">
        <div className="eyebrow">{t.commands}</div>
        <h1>{t.index}</h1>
        <p className="abstract">{t.lede(cmds.length)}</p>
      </section>
      {[...groups.entries()]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([g, list]) => (
          <section key={g} id={g}>
            <div className="sechead">
              <h2>{g}</h2>
            </div>
            <div className="tablewrap">
              <table>
                <tbody>
                  {list
                    .sort((a, b) => a.name.localeCompare(b.name))
                    .map((c) => (
                      <tr key={c.name}>
                        <td style={{ whiteSpace: 'nowrap' }}>
                          <a href={`${c.name.toLowerCase().replace(/[^a-z0-9.]/g, '-')}/`}>
                            <code>{c.name}</code>
                          </a>
                        </td>
                        <td>{c.summary}</td>
                      </tr>
                    ))}
                </tbody>
              </table>
            </div>
          </section>
        ))}
    </Chrome>,
  )
  return doc(
    lang,
    t.index,
    t.lede(cmds.length),
    `${lang === 'en' ? '' : `/${lang}`}/docs/commands/`,
    depth,
    css,
    body,
  )
}

export function renderCommandPage(
  lang: Lang,
  c: Command,
  version: string,
  depth: number,
  css: string,
): string {
  const t = L[lang]
  const rows: [string, React.ReactNode][] = [
    [t.syntax, <code>{c.syntax}</code>],
    [t.group, c.group],
    [t.arity, String(c.arity)],
    [t.since, c.since],
  ]
  if (c.flags.length) rows.push([t.flags, c.flags.join(', ')])
  if (c.complexity) rows.push([t.complexity, c.complexity])
  if (c.compat) rows.push([t.compat, c.compat])

  const body = renderToStaticMarkup(
    <Chrome lang={lang} depth={depth} version={version} here={c.name}>
      <section className="frontmatter">
        <div className="breadcrumb">
          <a href={up(depth) + (lang === 'en' ? '' : `${lang}/`)}>kevy</a>
          {' / '}
          <a href="../">{t.commands}</a>
        </div>
        <h1>
          <code>{c.name}</code>
        </h1>
        <p className="abstract">{c.summary}</p>
        <div className="tablewrap" style={{ marginTop: '2rem' }}>
          <table>
            <tbody>
              {rows.map(([k, v]) => (
                <tr key={k}>
                  <td style={{ whiteSpace: 'nowrap', width: '1%' }}>{k}</td>
                  <td>{v}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="caption">
          <a href="../">← {t.all}</a>
        </p>
      </section>
    </Chrome>,
  )
  return doc(
    lang,
    c.name,
    c.summary,
    `${lang === 'en' ? '' : `/${lang}`}/docs/commands/${c.name.toLowerCase()}/`,
    depth,
    css,
    body,
  )
}
