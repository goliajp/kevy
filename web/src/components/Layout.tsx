import { renderToStaticMarkup } from 'react-dom/server'

import { Footer } from './Footer'

// THE page shell. Every page on the site is this component with different
// children — the landing page, the documentation index, 105 reference
// pages, 577 command pages, 32 written pages.
//
// It exists because there were five of them: one in App.tsx and one each
// in entry-docs, entry-commands and entry-pages (twice). They drifted the
// day they were written. The documentation index offered "Home" where
// every other page offered "Home | Docs"; the language control rendered
// as a styled segmented button on the landing page and as three unstyled
// links everywhere else, because the stylesheet only ever named `button`;
// and the `<head>` boilerplate was copied four times, so a change to the
// font stack or the favicon would have had to be made four times to hold.
//
// One real difference survives, and it is behavioural: the landing page
// has a single URL and switches language in place, while every other page
// has a translated twin at a known URL and should offer a link a reader
// can open in a new tab. That is the `langs` prop.

export type Lang = 'en' | 'zh' | 'ja'

export const LANG_LABEL: Record<Lang, string> = { en: 'EN', zh: '中文', ja: '日本語' }
export const LANG_HTML: Record<Lang, string> = { en: 'en', zh: 'zh-CN', ja: 'ja' }
/** Every locale the site is built in. English is the one without a prefix. */
export const ALL_LANGS: Lang[] = ['en', 'zh', 'ja']

const NAV: Record<Lang, { home: string; docs: string; commands: string }> = {
  en: { home: 'Home', docs: 'Docs', commands: 'Commands' },
  zh: { home: '首页', docs: '文档', commands: '命令' },
  ja: { home: 'ホーム', docs: 'ドキュメント', commands: 'コマンド' },
}

export type LangControl =
  /** Prerendered: each locale is its own URL. */
  | { kind: 'links'; href: (l: Lang) => string; have?: Lang[] }
  /** The landing page: one URL, switched in place. */
  | { kind: 'buttons'; current: Lang; onPick: (l: Lang) => void }

export type LayoutProps = {
  lang: Lang
  /** `../` repeated — how far this page sits from the site root. */
  root: string
  /** Which nav entry is this page, so it can be marked current. */
  here?: 'docs' | 'commands'
  langs: LangControl
  /** Extra nav entries the landing page adds for its own sections. */
  extraNav?: { href: string; label: React.ReactNode }[]
  children: React.ReactNode
  /** The reference pages put a sidebar beside the content; nothing else does. */
  aside?: React.ReactNode
}

export function Layout({
  lang,
  root,
  here,
  langs,
  extraNav,
  children,
  aside,
}: LayoutProps) {
  const t = NAV[lang]
  const langRoot = lang === 'en' ? root : `${root}${lang}/`
  const have = langs.kind === 'links' ? (langs.have ?? (['en', 'zh', 'ja'] as Lang[])) : []
  return (
    <>
      <header className="masthead">
        <div className="masthead-inner">
          <a className="brand" href={langRoot}>
            <img src={`${root}kevy-logo.svg`} alt="" width={26} height={26} />
            <span className="wordmark">kevy</span>
          </a>
          <nav className="topnav">
            {extraNav?.map((n, i) => (
              <a key={i} className="hide-sm" href={n.href}>
                {n.label}
              </a>
            ))}
            <a href={langRoot}>{t.home}</a>
            <a
              href={`${langRoot}docs/`}
              className={here === 'docs' ? 'on' : undefined}
              aria-current={here === 'docs' ? 'page' : undefined}
            >
              {t.docs}
            </a>
            <a
              href={`${langRoot}docs/commands/`}
              className={here === 'commands' ? 'on hide-sm' : 'hide-sm'}
              aria-current={here === 'commands' ? 'page' : undefined}
            >
              {t.commands}
            </a>
            <div className="langswitch" role="group" aria-label="language">
              {(['en', 'zh', 'ja'] as Lang[]).map((l) =>
                langs.kind === 'buttons' ? (
                  <button
                    key={l}
                    className={l === langs.current ? 'on' : undefined}
                    onClick={() => langs.onPick(l)}
                  >
                    {LANG_LABEL[l]}
                  </button>
                ) : // A language this page does not exist in is not offered:
                // an offer that 404s is worse than no offer.
                have.includes(l) ? (
                  <a
                    key={l}
                    className={l === lang ? 'on' : undefined}
                    href={langs.href(l)}
                    // Lower case, spread in. React's DOM property map turns
                    // `hrefLang` into `hreflang` when it renders a real
                    // element, but renderToStaticMarkup emits an unknown
                    // camelCase prop verbatim — and `hrefLang` is not an
                    // HTML attribute, so every browser ignores it.
                    {...{ hreflang: LANG_HTML[l] }}
                  >
                    {LANG_LABEL[l]}
                  </a>
                ) : null,
              )}
            </div>
          </nav>
        </div>
      </header>

      {aside ? (
        <div className="docshell">
          {aside}
          <main className="docmain">{children}</main>
        </div>
      ) : (
        <div className="shell">{children}</div>
      )}

      <div className="shell">
        <Footer lang={lang} root={root} />
      </div>
    </>
  )
}

function escapeAttr(s: string) {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
}

export type DocumentProps = {
  lang: Lang
  /** Stated in a meta tag so every page can be held to the manifest. */
  version: string
  title: string
  desc: string
  /** Absolute path on the site, e.g. `/docs/persistence/`. */
  canonical: string
  root: string
  css: string
  /**
   * Which locales this page exists in, for hreflang. Default: all of them.
   *
   * The URLs are DERIVED from `canonical`, not passed in. They used to be
   * passed in, and the result was that one of the five callers of `page`
   * computed them and four did not: 657 of 768 pages went out with no
   * hreflang at all, including both localised home pages and all 618
   * command pages. Nothing was broken — the field was simply optional and
   * easy to not think about. Deriving it from the path the page already
   * declares is what makes forgetting impossible.
   *
   * A locale is only worth naming here when the page is missing from one:
   * an hreflang pointing at a 404 is worse than no hreflang.
   */
  have?: Lang[]
  /** Inline script, for the one page that needs arithmetic. */
  script?: string
}

/** The HTML document around a Layout. One copy, for the same reason. */
export function documentHtml(p: DocumentProps, body: string): string {
  // The locale prefix comes off the front and each locale's own goes back
  // on: `/ja/docs/x/` is `/docs/x/` is `/zh/docs/x/`. English carries no
  // prefix, which is why it is the tail itself.
  const tail = p.lang === 'en' ? p.canonical : p.canonical.replace(`/${p.lang}`, '') || '/'
  const urlFor = (l: Lang) => `https://kevy.golia.jp${l === 'en' ? tail : `/${l}${tail}`}`
  const have = p.have ?? ALL_LANGS
  // Including itself: an annotation that omits the page it is on is not a
  // set of alternates, and search engines are documented to ignore the
  // whole group when the return link is missing.
  const alt = [
    ...have.map((l) => `<link rel="alternate" hreflang="${LANG_HTML[l]}" href="${urlFor(l)}">`),
    // Whoever the other two are not for.
    ...(have.includes('en') ? [`<link rel="alternate" hreflang="x-default" href="${urlFor('en')}">`] : []),
  ].join('\n    ')
  return `<!doctype html>
<html lang="${LANG_HTML[p.lang]}">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>${escapeAttr(p.title)}</title>
    <meta name="description" content="${escapeAttr(p.desc)}">
    <link rel="canonical" href="https://kevy.golia.jp${p.canonical}">
    ${alt}
    <meta name="generator" content="kevy ${p.version}">
    <meta name="color-scheme" content="light">
    <meta name="theme-color" content="#fcfbf8">
    <link rel="icon" href="${p.root}kevy-logo.svg" type="image/svg+xml">
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap" rel="stylesheet">
    <link rel="stylesheet" href="${p.root}${p.css}">
  </head>
  <body>
${body}${p.script ? `\n    <script>\n${p.script}\n    </script>` : ''}
  </body>
</html>
`
}

/** Render a Layout to a complete HTML document. */
export function page(doc: DocumentProps, layout: React.ReactElement): string {
  return documentHtml(doc, renderToStaticMarkup(layout))
}
