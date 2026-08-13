import { ArrowDown, ArrowUpRight, Package } from 'lucide-react'
import { useEffect, useState } from 'react'

import { CodeBlock } from './components/CodeBlock'
import { Footer } from './components/Footer'
import { Terminal } from './components/Terminal'
import { detectLang, LANGS, LangContext, T, type Lang } from './i18n'

const GITHUB = 'https://github.com/goliajp/kevy'
const CRATES = 'https://crates.io/crates/kevy'
const DOCSRS = 'https://docs.rs/kevy'
const DOCS = '/docs/'

const LINKS = [
  { label: 'GitHub', href: GITHUB, icon: <Package size={14} strokeWidth={2} /> },
  { label: 'crates.io', href: CRATES, icon: <Package size={14} strokeWidth={2} /> },
  { label: 'docs.rs', href: DOCSRS, icon: <Package size={14} strokeWidth={2} /> },
]

const SERVER_SNIPPET = `# a server on the Redis port, speaking RESP2 and RESP3
cargo install kevy
kevy --port 6379

# your client does not know the difference
redis-cli SET user:1 alice
redis-cli IDX.CREATE users ON HASH PREFIX user: FIELDS city`

const EMBED_SNIPPET = `// the same engine, in your process — no server, no socket
let db = kevy_embedded::Store::open("data/")?;
db.set(b"user:1", b"alice", None)?;

// python: pip install kevy   ·   go: go get github.com/goliajp/kevy-go/v5
// flutter: flutter pub add flutter_kevy   ·   npm: @goliapkg/kevy`

// Measured, not claimed. bench/REPORT.md, precision bench: one million
// operations per run, ten runs, median. The report also carries the
// workloads where kevy does not win, and this table links to it rather
// than quietly ending at the last row it likes.
const PERF: { op: string; kevy: string; valkey: string; ratio: string }[] = [
  { op: 'SET, single connection', kevy: '94.7 k', valkey: '62.2 k', ratio: '1.52×' },
  { op: 'GET, single connection', kevy: '97.3 k', valkey: '65.0 k', ratio: '1.50×' },
  { op: 'SET, 50 conns pipelined', kevy: '4.11 M', valkey: '1.75 M', ratio: '2.35×' },
  { op: 'GET, 50 conns pipelined', kevy: '4.35 M', valkey: '3.42 M', ratio: '1.27×' },
]

const BEYOND = ['vector', 'fts', 'idx', 'view', 'feed', 'embed'] as const

function Section({
  id,
  title,
  lede,
  children,
}: {
  id: string
  title: React.ReactNode
  lede?: React.ReactNode
  children: React.ReactNode
}) {
  return (
    <section id={id}>
      <div className="sechead">
        <h2>{title}</h2>
        {lede && <p className="lede">{lede}</p>}
      </div>
      {children}
    </section>
  )
}

export function App() {
  const [lang, setLang] = useState<Lang>('en')
  useEffect(() => {
    setLang(detectLang())
  }, [])
  useEffect(() => {
    document.documentElement.lang = lang === 'zh' ? 'zh-CN' : lang
  }, [lang])

  return (
    <LangContext.Provider value={lang}>
      <header className="masthead">
        <div className="masthead-inner">
          <a className="brand" href="/">
            <span className="wordmark">kevy</span>
            <span className="ver">{__KEVY_VERSION__}</span>
          </a>
          <nav className="topnav">
            <a className="navlink" href="#try">
              <T k="nav.try" />
            </a>
            <a className="navlink hide-sm" href="#speed">
              <T k="nav.speed" />
            </a>
            <a className="navlink hide-sm" href="#beyond">
              <T k="nav.beyond" />
            </a>
            <a className="navlink" href={DOCS}>
              <T k="nav.docs" />
            </a>
            <div className="langswitch" role="group" aria-label="language">
              {LANGS.map((l) => (
                <button
                  key={l.id}
                  className={lang === l.id ? 'on' : ''}
                  onClick={() => {
                    setLang(l.id)
                    localStorage.setItem('lang', l.id)
                  }}
                >
                  {l.label}
                </button>
              ))}
            </div>
          </nav>
        </div>
      </header>

      <div className="shell">
        <section className="frontmatter">
          <div className="eyebrow">
            <T k="front.eyebrow" />
          </div>
          <h1>
            <T k="front.title.a" />
            <em>
              <T k="front.title.b" />
            </em>
            <T k="front.title.c" />
          </h1>
          <p className="abstract">
            <T k="front.abstract" />
          </p>

          <div className="figures">
            <div className="figure">
              <div className="v">2.35×</div>
              <div className="k">
                <T k="front.fig.speed" />
              </div>
            </div>
            <div className="figure">
              <div className="v">191</div>
              <div className="k">
                <T k="front.fig.commands" />
              </div>
            </div>
            <div className="figure">
              <div className="v">0</div>
              <div className="k">
                <T k="front.fig.deps" />
              </div>
            </div>
            <div className="figure">
              <div className="v">8</div>
              <div className="k">
                <T k="front.fig.langs" />
              </div>
            </div>
          </div>

          <div className="actions">
            <a className="btn primary" href="#try">
              <T k="front.cta.try" />
              <ArrowDown size={15} strokeWidth={2.25} />
            </a>
            {LINKS.map(({ label, href, icon }) => (
              <a key={label} className="btn" href={href} target="_blank" rel="noreferrer">
                {icon}
                {label}
                <ArrowUpRight size={14} strokeWidth={2} className="ext" />
              </a>
            ))}
          </div>
        </section>

        <Section id="try" title={<T k="term.heading" />} lede={<T k="term.blurb" />}>
          <Terminal />
          <p className="caption">
            <b>
              <T k="term.caption.label" />
            </b>{' '}
            <T k="term.caption" />
          </p>
        </Section>

        <Section id="speed" title={<T k="perf.heading" />} lede={<T k="perf.blurb" />}>
          <div className="tablewrap">
            <table>
              <thead>
                <tr>
                  <th>
                    <T k="perf.col.op" />
                  </th>
                  <th className="num">
                    <T k="perf.col.kevy" />
                  </th>
                  <th className="num">
                    <T k="perf.col.valkey" />
                  </th>
                  <th className="num">
                    <T k="perf.col.ratio" />
                  </th>
                </tr>
              </thead>
              <tbody>
                {PERF.map((r) => (
                  <tr key={r.op}>
                    <td>{r.op}</td>
                    <td className="num">{r.kevy}</td>
                    <td className="num">{r.valkey}</td>
                    <td className="num win">{r.ratio}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="caption">
            <b>
              <T k="perf.caption.label" />
            </b>{' '}
            <T k="perf.caption" />{' '}
            <a href="/benchmarks/">
              <T k="perf.report" /> →
            </a>
          </p>
        </Section>

        <Section id="beyond" title={<T k="more.heading" />} lede={<T k="more.blurb" />}>
          <div className="cards">
            {BEYOND.map((k) => (
              <div className="card" key={k}>
                <h3>
                  <T k={`more.${k}.h`} />
                </h3>
                <p>
                  <T k={`more.${k}.p`} />
                </p>
              </div>
            ))}
          </div>
        </Section>

        <Section id="install" title={<T k="inst.heading" />}>
          <div className="install-grid">
            <p className="prose">
              <T k="inst.server.blurb" />
            </p>
            <p className="prose">
              <T k="inst.embed.blurb" />
            </p>
            <CodeBlock
              label="cargo install kevy"
              copy="cargo install kevy"
              src={SERVER_SNIPPET}
              lang="sh"
            />
            <CodeBlock
              label="cargo add kevy-embedded"
              copy="cargo add kevy-embedded"
              src={EMBED_SNIPPET}
              lang="rust"
            />
          </div>
          <p className="caption">
            <T k="inst.docs" />{' '}
            <a href={DOCS}>kevy.golia.jp/docs</a>
          </p>
        </Section>

        <Footer lang={lang} />
      </div>
    </LangContext.Provider>
  )
}
