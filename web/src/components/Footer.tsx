import { ArrowUpRight, Package } from 'lucide-react'

import { Brand } from './Brand'

// One footer for every page on the site, and the same footer
// tiktoken.golia.jp carries — the two lab pages are one publication.
//
// The organisation mark is the GOLIA wordmark rather than the word: the
// same image file, from the same place, so the two pages close the same
// way.

export type Lang = 'en' | 'zh' | 'ja'

// Never a typed year. On the landing page this evaluates in the reader's
// browser; on the prerendered pages it evaluates at build time, and the
// release gate rebuilds the site every release. A hardcoded one goes
// wrong on 1 January and stays wrong.
const LICENSE: Record<Lang, (y: number) => string> = {
  en: (y) => `MIT or Apache-2.0 · © ${y} GOLIA K.K.`,
  zh: (y) => `MIT 或 Apache-2.0 · © ${y} GOLIA K.K.`,
  ja: (y) => `MIT または Apache-2.0 · © ${y} GOLIA K.K.`,
}

// crates.io has no simple-icons entry; lucide's Package is what a crate
// is, and it reads consistently beside the others.
export const LINKS = [
  { label: 'GitHub', href: 'https://github.com/goliajp/kevy', icon: <Brand name="GitHub" /> },
  {
    label: 'crates.io',
    href: 'https://crates.io/crates/kevy',
    icon: <Package size={14} strokeWidth={2} />,
  },
  { label: 'npm', href: 'https://www.npmjs.com/package/@goliapkg/kevy', icon: <Brand name="Npm" /> },
  { label: 'docs.rs', href: 'https://docs.rs/kevy', icon: <Brand name="DocsRs" /> },
]

export function Footer({ lang, root = './' }: { lang: Lang; root?: string }) {
  return (
    <footer>
      <div>
        <a
          className="org"
          href="https://golia.jp"
          target="_blank"
          rel="noreferrer"
          aria-label="GOLIA"
        >
          <img src={`${root}golia-wordmark.png`} alt="GOLIA" width={92} height={20} />
        </a>
        <div>{LICENSE[lang](new Date().getFullYear())}</div>
      </div>
      <div className="links">
        {LINKS.map(({ label, href, icon }) => (
          <a key={label} href={href} target="_blank" rel="noreferrer">
            {icon}
            {label}
            <ArrowUpRight size={12} strokeWidth={2} className="ext" />
          </a>
        ))}
      </div>
    </footer>
  )
}
