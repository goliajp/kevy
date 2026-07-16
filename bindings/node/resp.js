// RESP decode for kevy replies. One complete reply in, a JS value out:
//   +OK            -> "OK"
//   -ERR …         -> KevyError (returned, not thrown — the engine saying
//                     no is data; throwing is reserved for ABI misuse)
//   :42            -> 42n truncated to Number when safe, else BigInt
//   $N …           -> Uint8Array (call text() helper for strings)
//   $-1 / *-1      -> null
//   *N …           -> Array

// Extends Error so the taxonomy survives a `throw` intact: the typed surface
// throws this value directly (index.js), and `instanceof KevyError` — and
// `instanceof Error` — both hold, with a real stack. Mirrors bindings/ts.
export class KevyError extends Error {
  constructor(message) {
    super(message);
    this.name = "KevyError";
  }
}

const dec = new TextDecoder();

export function text(v) {
  return v instanceof Uint8Array ? dec.decode(v) : v;
}

export function parse(buf) {
  const [v, used] = one(buf, 0);
  if (used !== buf.length) throw new Error("kevy: trailing RESP bytes");
  return v;
}

function one(b, at) {
  const nl = line(b, at + 1);
  const head = dec.decode(b.subarray(at + 1, nl));
  const after = nl + 2;
  switch (b[at]) {
    case 0x2b /* + */:
      return [head, after];
    case 0x2d /* - */:
      return [new KevyError(head), after];
    case 0x3a /* : */: {
      const n = BigInt(head);
      const safe = n >= BigInt(Number.MIN_SAFE_INTEGER) && n <= BigInt(Number.MAX_SAFE_INTEGER);
      return [safe ? Number(n) : n, after];
    }
    case 0x24 /* $ */: {
      const n = parseInt(head, 10);
      if (n < 0) return [null, after];
      return [b.subarray(after, after + n), after + n + 2];
    }
    case 0x2a /* * */: {
      const n = parseInt(head, 10);
      if (n < 0) return [null, after];
      const items = [];
      let pos = after;
      for (let i = 0; i < n; i++) {
        const [item, next] = one(b, pos);
        items.push(item);
        pos = next;
      }
      return [items, pos];
    }
    default:
      // RESP2 only: the embedded cmd() path always replies in RESP2. A future
      // RESP3 tag (_ = null, # = bool, , = double, % = map, …) would land here.
      throw new Error(`kevy: unknown RESP tag ${b[at]}`);
  }
}

function line(b, from) {
  for (let i = from; i + 1 < b.length; i++) {
    if (b[i] === 13 && b[i + 1] === 10) return i;
  }
  throw new Error("kevy: truncated RESP reply");
}
