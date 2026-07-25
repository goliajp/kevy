// The decoded RESP reply the plugin's `cmd` command returns.
//
// Mirrors the Rust `JsonReply` (see tauri-plugin-kevy/src/reply.rs) and the
// `Reply` taxonomy in docs/client-contract.md §4.1. Byte payloads arrive as
// JSON number arrays for binary safety; the helpers below turn them back into
// `Uint8Array` / `string`.

/** A byte payload as it crosses the IPC boundary: a plain array of 0–255. */
export type ByteArray = number[]

/** The decoded reply, tagged by `type` (lowercase, matching the wire). */
export type Reply =
  | { type: 'simple'; bytes: ByteArray }
  | { type: 'error'; bytes: ByteArray }
  | { type: 'int'; int: number }
  | { type: 'bulk'; bytes: ByteArray }
  | { type: 'nil' }
  | { type: 'array'; items: Reply[] }
  | { type: 'map'; pairs: [Reply, Reply][] }
  | { type: 'set'; items: Reply[] }
  | { type: 'double'; double: number }
  | { type: 'boolean'; boolean: boolean }
  | { type: 'verbatim'; format: string; bytes: ByteArray }
  | { type: 'bignumber'; bytes: ByteArray }
  | { type: 'null' }
  | { type: 'push'; items: Reply[] }
  | { type: 'bloberror'; bytes: ByteArray }

const dec = new TextDecoder()
const enc = new TextEncoder()

/** Coerce a key/value argument (`string` | bytes) into the wire byte array. */
export function toByteArray(v: string | Uint8Array | ArrayBuffer | number[]): ByteArray {
  if (typeof v === 'string') return Array.from(enc.encode(v))
  if (v instanceof Uint8Array) return Array.from(v)
  if (v instanceof ArrayBuffer) return Array.from(new Uint8Array(v))
  return v
}

/** A reply's byte payload as a `Uint8Array`, or `null` for nil/null. */
export function replyBytes(r: Reply): Uint8Array | null {
  switch (r.type) {
    case 'bulk':
    case 'simple':
    case 'error':
    case 'verbatim':
    case 'bignumber':
    case 'bloberror':
      return Uint8Array.from(r.bytes)
    case 'nil':
    case 'null':
      return null
    default:
      throw new TypeError(`reply is ${r.type}, not a byte payload`)
  }
}

/** A reply's byte payload as a UTF-8 string, or `null` for nil/null. */
export function replyString(r: Reply): string | null {
  const b = replyBytes(r)
  return b === null ? null : dec.decode(b)
}

/** A reply's integer value. Throws if the reply is not an `int`. */
export function replyInt(r: Reply): number {
  if (r.type === 'int') return r.int
  throw new TypeError(`reply is ${r.type}, not an int`)
}

/** A reply's array items. Throws if the reply is not an `array`/`set`/`push`. */
export function replyArray(r: Reply): Reply[] {
  if (r.type === 'array' || r.type === 'set' || r.type === 'push') return r.items
  throw new TypeError(`reply is ${r.type}, not an array`)
}

/** Map an array reply to `(Uint8Array | null)[]` — the common bulk-list shape. */
export function replyBytesList(r: Reply): (Uint8Array | null)[] {
  return replyArray(r).map(replyBytes)
}

/** Map an array reply to `string[]` (dropping nils). */
export function replyStringList(r: Reply): string[] {
  return replyArray(r)
    .map(replyString)
    .filter((s): s is string => s !== null)
}
