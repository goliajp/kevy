import { Terminal } from './Terminal'

// The twelve block types the trilingual page content uses, rendered in the
// Golia Lab system. The content itself is exported verbatim from
// tools/site_content/{en,zh,ja}.py by tools/export_site_content.py and is
// never edited here — 4,600 lines of written and translated prose that a
// hand migration would have quietly lost some of.
//
// Nothing below interprets the text. Each block reads the fields the
// content already has and gives them the page's typography; where the old
// site had a bespoke treatment (a bar chart, a calculator), the block is a
// real component rather than a picture of one.

/** The content carries inline HTML — <b>, <code>, <a> — written by hand in
 *  the source dicts. It is repository content, not user input. */
function Html({ h, className }: { h: string; className?: string }) {
  return <span className={className} dangerouslySetInnerHTML={{ __html: h }} />
}

function BlockHead({ eyebrow, h2, intro }: { eyebrow?: string; h2?: string; intro?: string }) {
  if (!h2 && !intro && !eyebrow) return null
  return (
    <div className="sechead">
      {eyebrow && <div className="eyebrow">{eyebrow}</div>}
      {h2 && (
        <h2>
          <Html h={h2} />
        </h2>
      )}
      {intro && (
        <p className="lede">
          <Html h={intro} />
        </p>
      )}
    </div>
  )
}

type Any = Record<string, any>

function Hero({ b, version }: { b: Any; version: string }) {
  return (
    <section className="frontmatter">
      {/* The eyebrow used to be a typed "kevy 4.0" on a site serving 5.1.0.
          It carries the generated version now, and the content's own
          eyebrow text follows it. */}
      <div className="eyebrow">
        kevy {version}
        {b.eyebrow ? ` · ${String(b.eyebrow).replace(/^kevy\s+[\d.]+\s*·?\s*/i, '')}` : ''}
      </div>
      <h1>
        <Html h={b.h1} />
      </h1>
      {b.lede && (
        <p className="abstract">
          <Html h={b.lede} />
        </p>
      )}
      {Array.isArray(b.ctas) && b.ctas.length > 0 && (
        <div className="actions">
          {b.ctas.map((c: Any, i: number) => (
            <a key={c.href} className={i === 0 ? 'btn primary' : 'btn'} href={c.href}>
              {c.label}
            </a>
          ))}
        </div>
      )}
      {b.live_term && (
        <div style={{ marginTop: '2.4rem' }}>
          <Terminal />
        </div>
      )}
    </section>
  )
}

function Prose({ b }: { b: Any }) {
  const paras: string[] = Array.isArray(b.body) ? b.body : [b.body]
  return (
    <section>
      <BlockHead h2={b.h2} />
      {paras.filter(Boolean).map((p, i) => (
        <p className="prose" key={i} style={i ? { marginTop: '0.9rem' } : undefined}>
          <Html h={p} />
        </p>
      ))}
    </section>
  )
}

function Cards({ b }: { b: Any }) {
  return (
    <section>
      <BlockHead h2={b.h2} intro={b.intro} />
      <div className="cards">
        {(b.items ?? []).map((it: Any, i: number) => (
          <div className="card" key={i}>
            {it.kicker && <div className="cmd">{it.kicker}</div>}
            {it.title && (
              <h3>
                <Html h={it.title} />
              </h3>
            )}
            {it.body && (
              <p>
                <Html h={it.body} />
              </p>
            )}
            {it.href && (
              <p className="cmd">
                <a href={it.href}>{it.go ?? it.href} →</a>
              </p>
            )}
          </div>
        ))}
      </div>
    </section>
  )
}

function Table({ b }: { b: Any }) {
  return (
    <section>
      <BlockHead h2={b.h2} intro={b.intro} />
      <div className="tablewrap">
        <table>
          {b.head && (
            <thead>
              <tr>
                {b.head.map((h: string, i: number) => (
                  <th key={i} className={i ? 'num' : undefined}>
                    <Html h={h} />
                  </th>
                ))}
              </tr>
            </thead>
          )}
          <tbody>
            {(b.rows ?? []).map((r: string[], i: number) => (
              <tr key={i}>
                {r.map((c, j) => (
                  <td key={j} className={j ? 'num' : undefined}>
                    <Html h={c} />
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {b.note && (
        <p className="caption">
          <Html h={b.note} />
        </p>
      )}
    </section>
  )
}

/** Two measured series side by side, drawn as bars rather than described.
 *  The widths are a ratio of the largest value in the block, so the chart
 *  cannot flatter one side by choosing its own scale. */
function Bars({ b }: { b: Any }) {
  const rows: Any[] = b.rows ?? []
  const num = (v: unknown) => {
    const n = parseFloat(String(v).replace(/[^\d.]/g, ''))
    return Number.isFinite(n) ? n : 0
  }
  const max = Math.max(1, ...rows.flatMap((r) => [num(r.us ?? r[1]), num(r.them ?? r[2])]))
  return (
    <section id={b.id}>
      <BlockHead eyebrow={b.eyebrow} h2={b.h2} intro={b.intro} />
      <div className="bars">
        {rows.map((r: Any, i: number) => {
          const label = r.label ?? r[0]
          const us = r.us ?? r[1]
          const them = r.them ?? r[2]
          return (
            <div className="barrow" key={i}>
              <div className="barlabel">
                <Html h={String(label)} />
              </div>
              <div className="bartrack">
                <div className="bar us" style={{ width: `${(num(us) / max) * 100}%` }}>
                  <span>{us}</span>
                </div>
                <div className="bar them" style={{ width: `${(num(them) / max) * 100}%` }}>
                  <span>{them}</span>
                </div>
              </div>
            </div>
          )
        })}
      </div>
      <p className="caption">
        <b>{b.us ?? 'kevy'}</b> · {b.them ?? 'valkey'}
        {b.note ? ' — ' : ''}
        {b.note && <Html h={b.note} />}
      </p>
    </section>
  )
}

function Code({ b }: { b: Any }) {
  return (
    <section>
      <BlockHead h2={b.h2} />
      <div className="codeblock">
        <pre>
          <code>{b.text}</code>
        </pre>
      </div>
      {b.caption && (
        <p className="caption">
          <Html h={b.caption} />
        </p>
      )}
    </section>
  )
}

function Callout({ b }: { b: Any }) {
  const body: string[] = Array.isArray(b.body) ? b.body : [b.body]
  return (
    <section>
      <div className={`callout ${b.kind ?? ''}`}>
        {b.title && (
          <h3>
            <Html h={b.title} />
          </h3>
        )}
        {body.filter(Boolean).map((p, i) => (
          <p key={i}>
            <Html h={p} />
          </p>
        ))}
      </div>
    </section>
  )
}

function Steps({ b }: { b: Any }) {
  return (
    <section id={b.id}>
      <BlockHead h2={b.h2} intro={b.intro} />
      <ol className="steps">
        {(b.items ?? []).map((it: Any, i: number) => (
          <li key={i}>
            {it.title && (
              <h3>
                <Html h={it.title} />
              </h3>
            )}
            {it.body && (
              <p>
                <Html h={it.body} />
              </p>
            )}
            {it.code && (
              <pre>
                <code>{it.code}</code>
              </pre>
            )}
          </li>
        ))}
      </ol>
    </section>
  )
}

function Faq({ b }: { b: Any }) {
  return (
    <section>
      <BlockHead h2={b.h2} />
      <div className="faq">
        {(b.items ?? []).map((it: Any, i: number) => (
          <details key={i}>
            <summary>
              <Html h={it.q} />
            </summary>
            <p>
              <Html h={it.a} />
            </p>
          </details>
        ))}
      </div>
    </section>
  )
}

/** A worked example: the goal, the commands, and what it costs. The cost
 *  line is what separates a recipe from a snippet — a reader deciding
 *  whether to adopt a shape needs to know what it charges. */
function Recipe({ b }: { b: Any }) {
  return (
    <section>
      <BlockHead h2={b.h2} />
      {b.goal && (
        <p className="prose">
          <Html h={b.goal} />
        </p>
      )}
      <div className="recipe">
        {(b.items ?? []).map((it: Any, i: number) => (
          <div className="step" key={i}>
            {it.do && (
              <div className="rh">
                <Html h={it.do} />
              </div>
            )}
            {it.code && (
              <pre>
                <code>{it.code}</code>
              </pre>
            )}
            {it.note && (
              <p>
                <Html h={it.note} />
              </p>
            )}
          </div>
        ))}
      </div>
      {b.cost && (
        <p className="caption">
          <b>{b.cost_t ?? 'Cost'}</b> <Html h={b.cost} />
        </p>
      )}
    </section>
  )
}

/** Tabbed alternatives. Radio inputs and CSS rather than JavaScript: these
 *  pages are prerendered, and a tab that needs a bundle to open is a tab
 *  that does not open for a crawler or with scripting off. */
function Tabs({ b, uid }: { b: Any; uid: string }) {
  const items: Any[] = b.items ?? []
  return (
    <section id={b.id}>
      <BlockHead eyebrow={b.eyebrow} h2={b.h2} intro={b.intro} />
      <div className="tabs">
        {items.map((it: Any, i: number) => (
          <span key={i}>
            <input
              type="radio"
              name={uid}
              id={`${uid}-${i}`}
              defaultChecked={i === 0}
              className="tabinput"
            />
            <label htmlFor={`${uid}-${i}`} className="tablabel">
              {it.label}
            </label>
            <div className="tabpanel">
              {it.code && (
              <pre>
                <code>{it.code}</code>
              </pre>
            )}
              {it.note && (
                <p>
                  <Html h={it.note} />
                </p>
              )}
              {it.href && (
                <p className="caption">
                  <a href={it.href}>{it.go ?? it.href} →</a>
                </p>
              )}
            </div>
          </span>
        ))}
      </div>
    </section>
  )
}

/** The capacity calculator: the tiering ceiling, for the reader's data
 *  rather than for three example sizes.
 *
 *  The formula is the measured one from docs/tiering.md, carried over from
 *  the previous site unchanged and gated to ±20% by memgate:
 *
 *      max data:RAM ≈ value_size / (96 B + key heap)
 *
 *  Keys of 22 bytes or fewer live inline and cost no heap; 64 bytes is
 *  where a value is big enough for tiering to pay at all, because below it
 *  the stub is as large as the value it replaces. The arithmetic in the
 *  page is that one formula and nothing else — the site must not be able
 *  to claim something the gate does not measure.
 *
 *  `fields` is a dictionary of labels, not a list of inputs: the three
 *  inputs and three outputs are fixed by the formula, and what the content
 *  supplies is what to call them in each language. */
function Calc({ b }: { b: Any }) {
  const f: Any = b.fields ?? {}
  return (
    <section id="calc">
      <BlockHead h2={b.h2} intro={b.intro} />
      <form className="calc" onSubmit={(e) => e.preventDefault()}>
        <label>
          <span>{f.value}</span>
          <input type="number" defaultValue={4096} min={1} data-calc="value" />
          <em>B</em>
        </label>
        <label>
          <span>{f.key}</span>
          <input type="number" defaultValue={24} min={1} data-calc="key" />
          <em>B</em>
        </label>
        <label>
          <span>{f.budget}</span>
          <input type="number" defaultValue={32} min={1} data-calc="budget" />
          <em>GB</em>
        </label>
        <output data-calc-out data-l-ratio={f.ratio} data-l-served={f.served} data-l-below={f.below}>
          —
        </output>
      </form>
      {f.note && <p className="caption">{f.note}</p>}
    </section>
  )
}

export function Block({ b, version, uid }: { b: Any; version: string; uid: string }) {
  switch (b.t ?? b.kind) {
    case 'hero':
      return <Hero b={b} version={version} />
    case 'prose':
      return <Prose b={b} />
    case 'cards':
      return <Cards b={b} />
    case 'table':
      return <Table b={b} />
    case 'bars':
      return <Bars b={b} />
    case 'code':
      return <Code b={b} />
    case 'callout':
    case 'loss':
      return <Callout b={b} />
    case 'steps':
      return <Steps b={b} />
    case 'faq':
      return <Faq b={b} />
    case 'recipe':
      return <Recipe b={b} />
    case 'tabs':
      return <Tabs b={b} uid={uid} />
    case 'calc':
      return <Calc b={b} />
    default:
      // A block type nobody implemented must be visible, not skipped. A
      // silently dropped block is content that vanished from the site with
      // no diff to show for it — and the parity check downstream counts on
      // seeing it here.
      throw new Error(`unhandled block type: ${JSON.stringify(b.t ?? b.kind)}`)
  }
}
