// Non-atomic pipeline batching (contract §3.13). Remote-only. Queue N
// commands client-side, send as one write, read N replies in order. NOT
// atomic (the server may interleave other clients). Per-command errors come
// back as Reply::error entries INLINE (they do not abort the batch), unlike
// the typed single-command wraps. An empty batch touches no wire; an
// empty-argv command poisons the batch → InvalidInput at send.

import { encodeCommand, toBytes, type Data, type Reply } from "./resp.ts";
import { InvalidInputError } from "./errors.ts";
import type { RespConn } from "./transport.ts";

/** Accumulates commands for a pipeline. Chainable. */
export class PipelineBuf {
  #chunks: Uint8Array[] = [];
  #count = 0;
  #poisoned = false;

  /** Queue one command (verb + args); an empty parts poisons the batch. */
  cmd(...parts: Data[]): this {
    if (parts.length === 0) {
      this.#poisoned = true;
      return this;
    }
    this.#chunks.push(encodeCommand(parts.map(toBytes)));
    this.#count++;
    return this;
  }

  get length(): number {
    return this.#count;
  }
  isEmpty(): boolean {
    return this.#count === 0;
  }

  /** @internal */
  finish(): { wire: Uint8Array; count: number; poisoned: boolean } {
    let total = 0;
    for (const c of this.#chunks) total += c.length;
    const wire = new Uint8Array(total);
    let pos = 0;
    for (const c of this.#chunks) {
      wire.set(c, pos);
      pos += c.length;
    }
    return { wire, count: this.#count, poisoned: this.#poisoned };
  }
}

/** Run a pipelined batch over a remote connection (contract §3.13). */
export async function runPipeline(conn: RespConn, build: (p: PipelineBuf) => void): Promise<Reply[]> {
  const p = new PipelineBuf();
  build(p);
  const { wire, count, poisoned } = p.finish();
  if (poisoned) throw new InvalidInputError("pipeline: a queued command had an empty argv");
  if (count === 0) return [];
  return conn.pipelineRaw(wire, count);
}
