// RESP decode for kevy replies — the TypeScript twin of
// bindings/node/resp.js. One complete reply in, a JS value out:
//   +OK            -> "OK"
//   -ERR …         -> KevyError (returned, not thrown — the engine saying
//                     no is data; throwing is reserved for ABI misuse)
//   :42            -> 42 truncated to Number when safe, else BigInt
//   $N …           -> Uint8Array (call text() helper for strings)
//   $-1 / *-1      -> null
//   *N …           -> Array
//
// RESP2 only: kevy's FFI replies never negotiate RESP3, so the five tags
// above (+ - : $ *) are the whole grammar. The RESP3 tags (_ # , ( ! = % ~ >)
// are intentionally not handled and hit the unknown-tag path below.

export type KevyReply =
  | string
  | number
  | bigint
  | Uint8Array
  | KevyError
  | null
  | KevyReply[];

export class KevyError {
  readonly message: string;
  constructor(message: string) {
    this.message = message;
  }
}

const dec = new TextDecoder();

export function text(v: KevyReply): string | KevyReply {
  return v instanceof Uint8Array ? dec.decode(v) : v;
}

export function parse(buf: Uint8Array): KevyReply {
  const [v, used] = one(buf, 0);
  if (used !== buf.length) throw new Error("kevy: trailing RESP bytes");
  return v;
}

function one(b: Uint8Array, at: number): [KevyReply, number] {
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
      const items: KevyReply[] = [];
      let pos = after;
      for (let i = 0; i < n; i++) {
        const [item, next] = one(b, pos);
        items.push(item);
        pos = next;
      }
      return [items, pos];
    }
    default:
      throw new Error(`kevy: unknown RESP tag ${b[at]}`);
  }
}

function line(b: Uint8Array, from: number): number {
  for (let i = from; i + 1 < b.length; i++) {
    if (b[i] === 13 && b[i + 1] === 10) return i;
  }
  throw new Error("kevy: truncated RESP reply");
}
