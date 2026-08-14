import { Check, Copy } from 'lucide-react'
import { useState } from 'react'

// Syntax colour by regex, deliberately. A highlighter is 40 KB to make four
// snippets slightly prettier, and the two things worth distinguishing here
// are comments and strings — a reader scanning `cargo add kevy` does not
// need a parser's opinion about the rest.
function paint(src: string, lang: 'sh' | 'rust' | 'js') {
  const out: React.ReactNode[] = []
  const re =
    lang === 'sh'
      ? /(#[^\n]*)|("[^"]*")/g
      : /(\/\/[^\n]*)|("[^"]*"|'[^']*')|\b(let|const|await|import|from|fn|use|pub|async|new)\b/g
  let last = 0
  let m: RegExpExecArray | null
  let k = 0
  while ((m = re.exec(src))) {
    if (m.index > last) out.push(src.slice(last, m.index))
    const cls = m[1] ? 'tok-c' : m[2] ? 'tok-s' : 'tok-k'
    out.push(
      <span key={k++} className={cls}>
        {m[0]}
      </span>,
    )
    last = m.index + m[0].length
  }
  out.push(src.slice(last))
  return out
}

export function CodeBlock({
  label,
  copy,
  src,
  lang,
  className,
}: {
  label: string
  copy?: string
  src: string
  lang: 'sh' | 'rust' | 'js'
  className?: string
}) {
  const [done, setDone] = useState(false)
  return (
    <div className={className ? `codeblock ${className}` : 'codeblock'}>
      <div className="code-head">
        <span>{label}</span>
        {copy && (
          <button
            type="button"
            onClick={() => {
              void navigator.clipboard.writeText(copy).then(() => {
                setDone(true)
                setTimeout(() => setDone(false), 1400)
              })
            }}
            aria-label="copy"
          >
            {done ? <Check size={12} /> : <Copy size={12} />}
          </button>
        )}
      </div>
      <pre>
        <code>{paint(src, lang)}</code>
      </pre>
    </div>
  )
}
