import { ArrowUpRight, Package } from 'lucide-react'

// One footer for every page on the site. The landing page renders it in the
// browser, the reference pages render it at build time through the same
// component — so there is no second copy to fall out of step with the first.

export type Lang = 'en' | 'zh' | 'ja'

// Never a typed year. On the landing page this evaluates in the reader's
// browser, so it is right by definition; on the reference pages it
// evaluates at build time, and the site is rebuilt on every release, which
// the release gate now enforces. A hardcoded one goes wrong on 1 January
// and stays wrong until somebody notices, which is the same shape as the
// version numbers that read 4.0 and 5.0 on a site serving 5.1.0.
const LICENSE: Record<Lang, (y: number) => string> = {
  en: (y) => `MIT or Apache-2.0 · © ${y} GOLIA K.K.`,
  zh: (y) => `MIT 或 Apache-2.0 · © ${y} GOLIA K.K.`,
  ja: (y) => `MIT または Apache-2.0 · © ${y} GOLIA K.K.`,
}

export const LINKS = [
  { label: 'GitHub', href: 'https://github.com/goliajp/kevy' },
  { label: 'crates.io', href: 'https://crates.io/crates/kevy' },
  { label: 'docs.rs', href: 'https://docs.rs/kevy' },
]

export function Footer({ lang }: { lang: Lang }) {
  return (
    <footer>
      <div>
        <a className="org" href="https://golia.jp" target="_blank" rel="noreferrer">
          GOLIA
        </a>
        <div>{LICENSE[lang](new Date().getFullYear())}</div>
      </div>
      <div className="links">
        {LINKS.map(({ label, href }) => (
          <a key={label} href={href} target="_blank" rel="noreferrer">
            <Package size={13} strokeWidth={2} />
            {label}
            <ArrowUpRight size={12} strokeWidth={2} className="ext" />
          </a>
        ))}
      </div>
    </footer>
  )
}
