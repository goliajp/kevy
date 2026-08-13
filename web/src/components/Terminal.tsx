import { useEffect, useRef, useState } from 'react'

import { T, t, useLang } from '../i18n'

// The engine, in the page. `open()` instantiates the wasm module and hands
// back something that answers `cmd(...args)` with a decoded RESP reply —
// the same reply the server would put on a socket.
//
// Persistence is off here on purpose. The landing page's terminal should be
// empty for every visitor, so what they see is what they typed; the full
// playground at /play/ opens the same engine with OPFS behind it and keeps
// state across reloads. One engine, two configurations.
type Reply = unknown
type Engine = { cmd: (...args: string[]) => Reply; close: () => Promise<void> }

type Line = { kind: 'in' | 'out' | 'err'; text: string }

const SEED: string[] = [
  'SET user:1 alice',
  'GET user:1',
  'LPUSH jobs "resize image"',
  'ZADD board 42 alice',
  'ZRANGE board 0 -1 WITHSCORES',
  'IDX.CREATE users ON HASH PREFIX user: FIELDS city',
  'DBSIZE',
]

// RESP replies are data, so rendering one is a fold over the shapes the
// decoder produces rather than a switch on a wire type. Errors arrive as
// values too (a KevyError instance), which is why the engine rejecting a
// verb prints like any other answer instead of blowing up the terminal.
function render(v: Reply, depth = 0): string {
  if (v === null || v === undefined) return '(nil)'
  if (v instanceof Error) return `(error) ${v.message}`
  if (v instanceof Uint8Array) return JSON.stringify(new TextDecoder().decode(v))
  if (typeof v === 'string') return v
  if (typeof v === 'number' || typeof v === 'bigint') return `(integer) ${v}`
  if (Array.isArray(v)) {
    if (v.length === 0) return '(empty array)'
    const pad = '  '.repeat(depth)
    return v.map((x, i) => `${pad}${i + 1}) ${render(x, depth + 1)}`).join('\n')
  }
  return String(v)
}

export function Terminal() {
  const lang = useLang()
  const [lines, setLines] = useState<Line[]>([])
  const [state, setState] = useState<'boot' | 'live' | 'dead'>('boot')
  const engine = useRef<Engine | null>(null)
  const body = useRef<HTMLDivElement>(null)
  const input = useRef<HTMLInputElement>(null)

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

  function run(cmd: string) {
    const trimmed = cmd.trim()
    if (!trimmed) return
    const db = engine.current
    if (!db) return
    const next: Line[] = [{ kind: 'in', text: trimmed }]
    try {
      // Quoted arguments hold together; everything else splits on runs of
      // whitespace. A shell-accurate parser is not the point — the point is
      // that `SET k "two words"` does what a reader expects.
      const args = (trimmed.match(/"[^"]*"|\S+/g) ?? []).map((a) =>
        a.startsWith('"') && a.endsWith('"') ? a.slice(1, -1) : a,
      )
      const reply = db.cmd(...args)
      const text = render(reply)
      next.push({ kind: reply instanceof Error ? 'err' : 'out', text })
    } catch (e) {
      next.push({ kind: 'err', text: String(e) })
    }
    setLines((prev) => [...prev, ...next])
  }

  return (
    <>
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
            placeholder={state === 'live' ? 'SET k v' : t('term.booting', lang)}
            spellCheck={false}
            autoComplete="off"
            aria-label="command"
          />
        </form>
      </div>
      <div className="term-chips">
        {SEED.map((c) => (
          <button key={c} type="button" disabled={state !== 'live'} onClick={() => run(c)}>
            {c}
          </button>
        ))}
      </div>
    </>
  )
}
