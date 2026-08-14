import { useEffect, useRef, useState } from 'react'

import { T, phrase, t, useLang } from '../i18n'
import { SCENARIOS } from '../scenarios'

// The engine, in the page. `open()` instantiates the wasm module and hands
// back something that answers `cmd(...args)` with a decoded RESP reply —
// the same reply the server would put on a socket. The wasm is built from
// this checkout (web/engine.mjs), not from the published package, so what
// the page demonstrates is what this commit does.
//
// There is no second, bigger playground somewhere else — this is it, in
// the same shell as every other page, because a demo behind its own
// fullscreen surface is a demo most readers never open.
//
// Two things are deliberately absent.
//
// **Persistence.** The wasm build's durability is host-mediated: the typed
// setters push frames to the OPFS worker, and `cmd` does not — it goes
// straight to `Store::dispatch_argv`. Measured, not assumed: write through
// `cmd`, close, reopen, and the key is gone, while a typed write survives.
// So a "persist" switch on a *command* terminal would invite a reader to
// type SET, reload, and conclude kevy loses data. Better absent than lying.
//
// **Streams, transactions, geo, scripting.** The embedded engine's verb
// surface is the ESTORE_OPS manifest — 112 of the 191 verbs the server
// answers. Those four groups are not in it. Nothing here is trimmed to
// save bytes; the browser build enables every feature a browser can host.
type Reply = unknown
type Engine = { cmd: (...args: string[]) => Reply; close: () => Promise<void> }

type Line = { kind: 'in' | 'out' | 'err'; text: string }

// RESP replies are data, so rendering one is a fold over the shapes the
// decoder produces rather than a switch on a wire type. Errors arrive as
// values too (a KevyError instance), which is why the engine rejecting a
// verb prints like any other answer instead of blowing up the terminal.
//
// Nesting follows redis-cli: a child's first line continues its parent's
// numbered line, and its remaining lines indent by the width of that
// number. `IDX.QUERY … FIELDS` returns rows of [key, field, value …]
// inside a [cursor, rows] envelope, which is three levels deep — printed
// flat it is unreadable, and the query results are the whole point.
function render(v: Reply): string {
  if (v === null || v === undefined) return '(nil)'
  if (v instanceof Error) return `(error) ${v.message}`
  if (v instanceof Uint8Array) return JSON.stringify(new TextDecoder().decode(v))
  if (typeof v === 'string') return v
  if (typeof v === 'number' || typeof v === 'bigint') return `(integer) ${v}`
  if (Array.isArray(v)) {
    if (v.length === 0) return '(empty array)'
    const width = String(v.length).length
    return v
      .map((x, i) => {
        const head = `${String(i + 1).padStart(width)}) `
        const [first, ...rest] = render(x).split('\n')
        return [head + first, ...rest.map((l) => ' '.repeat(head.length) + l)].join('\n')
      })
      .join('\n')
  }
  return String(v)
}

/** Quoted arguments hold together; everything else splits on runs of
 *  whitespace. A shell-accurate parser is not the point — the point is that
 *  `SET k "two words"` does what a reader expects. */
function argv(line: string): string[] {
  return (line.match(/"[^"]*"|\S+/g) ?? []).map((a) =>
    a.startsWith('"') && a.endsWith('"') ? a.slice(1, -1) : a,
  )
}

export function Terminal() {
  const lang = useLang()
  const [lines, setLines] = useState<Line[]>([])
  const [state, setState] = useState<'boot' | 'live' | 'dead'>('boot')
  const [scenario, setScenario] = useState<string | null>(null)
  const engine = useRef<Engine | null>(null)
  const body = useRef<HTMLDivElement>(null)
  const input = useRef<HTMLInputElement>(null)
  // Command history, newest last, walked with the arrow keys. `at` is the
  // position being edited: history.length means "the empty line at the end".
  const history = useRef<string[]>([])
  const at = useRef(0)

  useEffect(() => {
    let cancelled = false
    let opened: Engine | null = null
    ;(async () => {
      try {
        // Dynamic, so the wasm is fetched when this component mounts rather
        // than blocking the page shell. A visitor who never scrolls here
        // never pays for the engine.
        const mod = await import('@goliapkg/kevy')
        const db = (await mod.open({ persist: false })) as unknown as Engine
        if (cancelled) {
          void db.close()
          return
        }
        opened = db
        engine.current = db
        setState('live')
      } catch {
        // No error text on screen: the reason is a wasm instantiation
        // failure the visitor cannot act on. What they can act on is that
        // the rest of the page still works, so say only that.
        if (!cancelled) setState('dead')
      }
    })()
    return () => {
      cancelled = true
      void opened?.close()
    }
  }, [])

  useEffect(() => {
    // Follow the tail, the way a terminal does.
    body.current?.scrollTo({ top: body.current.scrollHeight })
  }, [lines])

  /** One command in, its echo and its reply out. */
  function once(cmd: string): Line[] {
    const db = engine.current
    if (!db) return []
    const out: Line[] = [{ kind: 'in', text: cmd }]
    try {
      const reply = db.cmd(...argv(cmd))
      out.push({ kind: reply instanceof Error ? 'err' : 'out', text: render(reply) })
    } catch (e) {
      out.push({ kind: 'err', text: String(e) })
    }
    return out
  }

  /** Run one line, or several — pasting a block of commands runs the block,
   *  which is how anyone with a recipe in front of them will use this. */
  function run(text_: string) {
    const cmds = text_
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean)
    if (!cmds.length || !engine.current) return
    for (const c of cmds) history.current.push(c)
    at.current = history.current.length
    setLines((prev) => [...prev, ...cmds.flatMap(once)])
  }

  function runScenario(id: string) {
    const s = SCENARIOS.find((x) => x.id === id)
    if (!s) return
    setScenario(id)
    // Each scenario builds its own data, so it starts from a clean store —
    // otherwise the second one a reader tries answers over the first one's
    // rows and the numbers stop meaning anything.
    engine.current?.cmd('FLUSHALL')
    setLines(s.lines.flatMap(once))
  }

  function onKey(e: React.KeyboardEvent<HTMLInputElement>) {
    const el = e.currentTarget
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
    const h = history.current
    if (!h.length) return
    e.preventDefault()
    at.current = Math.min(h.length, Math.max(0, at.current + (e.key === 'ArrowUp' ? -1 : 1)))
    el.value = at.current === h.length ? '' : h[at.current]
    el.setSelectionRange(el.value.length, el.value.length)
  }

  const chosen = SCENARIOS.find((s) => s.id === scenario)

  return (
    <>
      <div className="term-pick" role="group" aria-label={t('term.scenarios', lang)}>
        {SCENARIOS.map((s) => (
          <button
            key={s.id}
            type="button"
            disabled={state !== 'live'}
            className={s.id === scenario ? 'on' : undefined}
            onClick={() => runScenario(s.id)}
          >
            {phrase(s.label[lang], lang)}
          </button>
        ))}
      </div>
      <p className="term-blurb">
        {chosen ? phrase(chosen.blurb[lang], lang) : <T k="term.pick" />}
      </p>
      <div className="term">
        <div className="term-head">
          <span className="dot" />
          <span className="dot" />
          <span className="dot" />
          <span>kevy {__KEVY_VERSION__} · wasm</span>
          <span className={state === 'live' ? 'state live' : 'state'}>
            {state === 'boot' && <T k="term.booting" />}
            {state === 'live' && <T k="term.live" />}
            {state === 'dead' && <T k="term.failed" />}
          </span>
        </div>
        <div className="term-body" ref={body} aria-live="polite">
          {lines.map((l, i) => (
            <div key={i} className={l.kind}>
              {l.text}
            </div>
          ))}
        </div>
        <form
          className="term-form"
          onSubmit={(e) => {
            e.preventDefault()
            const el = input.current
            if (!el) return
            run(el.value)
            el.value = ''
          }}
        >
          <span>›</span>
          <input
            ref={input}
            disabled={state !== 'live'}
            placeholder={state === 'live' ? t('term.prompt', lang) : t('term.booting', lang)}
            spellCheck={false}
            autoComplete="off"
            aria-label="command"
            onKeyDown={onKey}
            onPaste={(e) => {
              // A pasted block goes straight through, rather than landing in
              // the input as one impossible line.
              const text_ = e.clipboardData.getData('text')
              if (!text_.includes('\n')) return
              e.preventDefault()
              run(text_)
            }}
          />
        </form>
      </div>
      <p className="term-foot">
        <button
          type="button"
          className="linkish"
          disabled={state !== 'live'}
          onClick={() => {
            engine.current?.cmd('FLUSHALL')
            setScenario(null)
            setLines([])
          }}
        >
          <T k="term.reset" />
        </button>
        <span> · </span>
        <T k="term.reach" />
      </p>
    </>
  )
}
