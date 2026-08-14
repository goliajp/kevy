import { type Lang, Layout, page } from './components/Layout'

// The command reference: one page per verb, plus an index, through the
// same Layout as everything else. Generated from web/src/commands.json,
// which the engine's own gen_docs binary writes out of VERB_META — a verb
// that gains a flag changes these pages by changing the code, and there is
// no second description to update.

export type { Lang }

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
    index: '命令参考',
    lede: (n: number) => `引擎能回答的每一条动词 —— 共 ${n} 条,与服务端分发所用的是同一张表。`,
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

const up = (depth: number) => '../'.repeat(depth)

/** A verb name can carry a dot (IDX.CREATE); the directory has to survive
 *  a filesystem and a URL both. */
export const slugOf = (name: string) => name.toLowerCase().replace(/[^a-z0-9.]/g, '-')

export function renderCommandIndex(
  lang: Lang,
  cmds: Command[],
  version: string,
  depth: number,
  css: string,
): string {
  const t = L[lang]
  const root = up(depth)
  const langRoot = (l: Lang) => (l === 'en' ? root : `${root}${l}/`)
  // Grouped: 191 verbs in one alphabetical run is a list nobody reads. The
  // groups come from the engine's own table.
  const groups = new Map<string, Command[]>()
  for (const c of cmds) {
    if (!groups.has(c.group)) groups.set(c.group, [])
    groups.get(c.group)!.push(c)
  }
  return page(
    {
      lang,
      title: `${t.index} · kevy`,
      desc: t.lede(cmds.length),
      canonical: `${lang === 'en' ? '' : `/${lang}`}/docs/commands/`,
      root,
      css,
    },
    <Layout
      lang={lang}
      version={version}
      root={root}
      here="commands"
      langs={{ kind: 'links', href: (l) => `${langRoot(l)}docs/commands/` }}
    >
      <section className="frontmatter">
        <div className="eyebrow">{t.index}</div>
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
                          <a href={`${slugOf(c.name)}/`}>
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
    </Layout>,
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
  const root = up(depth)
  const langRoot = (l: Lang) => (l === 'en' ? root : `${root}${l}/`)
  const rows: [string, React.ReactNode][] = [
    [t.syntax, <code>{c.syntax}</code>],
    [t.group, c.group],
    [t.arity, String(c.arity)],
    [t.since, c.since],
  ]
  if (c.flags.length) rows.push([t.flags, c.flags.join(', ')])
  if (c.complexity) rows.push([t.complexity, c.complexity])
  if (c.compat) rows.push([t.compat, c.compat])

  return page(
    {
      lang,
      title: `${c.name} · kevy`,
      desc: c.summary,
      canonical: `${lang === 'en' ? '' : `/${lang}`}/docs/commands/${slugOf(c.name)}/`,
      root,
      css,
    },
    <Layout
      lang={lang}
      version={version}
      root={root}
      here="commands"
      langs={{ kind: 'links', href: (l) => `${langRoot(l)}docs/commands/${slugOf(c.name)}/` }}
    >
      <section className="frontmatter">
        <div className="breadcrumb">
          <a href={langRoot(lang)}>kevy</a>
          {' / '}
          <a href="../">{t.all}</a>
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
    </Layout>,
  )
}
