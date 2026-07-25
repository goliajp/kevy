// Frontend for the kevy Tauri example.
//
// Uses the global `window.__TAURI__` (enabled by `withGlobalTauri` in
// tauri.conf.json) so this stays a static file with no bundler. In a real app
// you'd instead `import { kevy } from 'tauri-plugin-kevy-api'` (see
// bindings/tauri/guest-js) for a typed client — the calls below mirror exactly
// what that binding does under the hood.

const { invoke, Channel } = window.__TAURI__.core

const enc = new TextEncoder()
const dec = new TextDecoder()
// Keys/values cross the IPC boundary as byte arrays (binary-safe).
const bytes = (s) => Array.from(enc.encode(s))
const str = (arr) => (arr == null ? null : dec.decode(Uint8Array.from(arr)))
const $ = (id) => document.getElementById(id)

// ── set / get ──────────────────────────────────────────────────────────────
$('set').onclick = async () => {
  await invoke('plugin:kevy|set', { key: bytes($('key').value), value: bytes($('val').value), ttlMs: null })
  $('kvOut').textContent = `SET ${$('key').value} = ${$('val').value}`
}
$('get').onclick = async () => {
  const v = await invoke('plugin:kevy|get', { key: bytes($('key').value) })
  $('kvOut').textContent = v == null ? '(nil)' : str(v)
}

// ── pub/sub ──────────────────────────────────────────────────────────────────
let subId = null
$('sub').onclick = async () => {
  const channel = new Channel()
  channel.onmessage = (m) => {
    if (m.kind === 'subscribe') append(`✓ subscribed (${m.count} held)`)
    else if (m.kind === 'message') append(`← ${str(m.channel)}: ${str(m.payload)}`)
  }
  subId = await invoke('plugin:kevy|subscribe', {
    channels: [bytes($('chan').value)],
    patterns: [],
    onEvent: channel,
  })
  $('sub').disabled = true
  $('unsub').disabled = false
}
$('unsub').onclick = async () => {
  if (subId != null) await invoke('plugin:kevy|unsubscribe', { id: subId })
  subId = null
  $('sub').disabled = false
  $('unsub').disabled = true
  append('✗ unsubscribed')
}
$('pub').onclick = async () => {
  const n = await invoke('plugin:kevy|publish', { channel: bytes($('chan').value), payload: bytes($('msg').value) })
  append(`→ published to ${n} subscriber(s)`)
}
function append(line) {
  const el = $('feed')
  el.textContent += (el.textContent ? '\n' : '') + line
  el.scrollTop = el.scrollHeight
}

// ── raw command ──────────────────────────────────────────────────────────────
$('run').onclick = async () => {
  const parts = $('raw').value.trim().split(/\s+/).filter(Boolean)
  if (parts.length === 0) return
  const reply = await invoke('plugin:kevy|cmd', { argv: parts.map(bytes) })
  $('rawOut').textContent = render(reply)
}
function render(r) {
  switch (r.type) {
    case 'int': return `(integer) ${r.int}`
    case 'nil':
    case 'null': return '(nil)'
    case 'simple': return str(r.bytes)
    case 'error': return `(error) ${str(r.bytes)}`
    case 'bulk': return str(r.bytes)
    case 'array':
    case 'set':
    case 'push': return r.items.map((x, i) => `${i + 1}) ${render(x)}`).join('\n')
    default: return JSON.stringify(r)
  }
}
