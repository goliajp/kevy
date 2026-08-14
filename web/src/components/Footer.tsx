import { ArrowUpRight, Package } from 'lucide-react'

import { phrase } from '../i18n'
import { Brand } from './Brand'

// One footer for every page on the site, and the same footer
// tiktoken.golia.jp carries — the two lab pages are one publication.
//
// The organisation mark is the GOLIA wordmark rather than the word: the
// same image file, from the same place, so the two pages close the same
// way.

export type Lang = 'en' | 'zh' | 'ja'

// The same line tiktoken.golia.jp carries, in the same words: the GOLIA
// wordmark above it already says whose publication this is, so a second
// "GOLIA K.K." beside a year says it twice and dates the page besides.
const LICENSE: Record<Lang, string> = {
  en: 'Released under MIT OR Apache-2.0',
  zh: '以 MIT OR Apache-2.0 双许可发布',
  ja: 'MIT OR Apache-2.0 のデュアルライセンスで公開',
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
        <div>{phrase(LICENSE[lang], lang)}</div>
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
