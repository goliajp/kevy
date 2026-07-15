// The remote transport: a native RESP2/3 client over node:net (which Bun
// implements too, so one path serves both runtimes). Async — remote sync is
// not idiomatic in JS (contract §1.4 / §7). A single read buffer reassembles
// multi-segment replies; a FIFO waiter queue resolves replies in wire order
// so sequential requests and pipelines both work. Not safe for concurrent
// use beyond the ordered pipelining the client drives.

import net from "node:net";
import { encodeCommand, parseReply, type Reply } from "./resp.ts";
import { ClosedError, IoError, ProtocolError, TimedOutError, type KevyError } from "./errors.ts";

interface Waiter {
  resolve: (r: Reply) => void;
  reject: (e: unknown) => void;
  timer?: ReturnType<typeof setTimeout>;
}

const EMPTY = new Uint8Array(0);

export class RespConn {
  #sock: net.Socket;
  #buf: Uint8Array = EMPTY;
  #waiters: Waiter[] = [];
  #closed = false;
  #closeErr: KevyError | null = null;
  #readTimeout: number | null = null;

  private constructor(sock: net.Socket) {
    this.#sock = sock;
    sock.setNoDelay(true);
    sock.on("data", (chunk: Buffer) => this.#onData(chunk));
    sock.on("error", (err) => this.#fail(new IoError(err.message, err)));
    sock.on("close", () => this.#fail(this.#closeErr ?? new ClosedError()));
    sock.on("end", () => this.#fail(new ClosedError()));
  }

  /** Dial host:port with TCP_NODELAY. */
  static dial(host: string, port: number): Promise<RespConn> {
    return new Promise((resolve, reject) => {
      const sock = net.connect({ host, port });
      const onErr = (err: Error) => reject(new IoError(err.message, err));
      sock.once("error", onErr);
      sock.once("connect", () => {
        sock.removeListener("error", onErr);
        resolve(new RespConn(sock));
      });
    });
  }

  #onData(chunk: Buffer): void {
    this.#buf = this.#buf.length === 0 ? new Uint8Array(chunk) : concat(this.#buf, chunk);
    this.#drain();
  }

  #drain(): void {
    while (this.#waiters.length > 0) {
      let r: Reply;
      let used: number;
      try {
        [r, used] = parseReply(this.#buf, 0);
      } catch {
        this.#fail(new ProtocolError("malformed reply"));
        return;
      }
      if (used === 0) break; // need more bytes
      this.#buf = used === this.#buf.length ? EMPTY : this.#buf.subarray(used);
      const w = this.#waiters.shift()!;
      if (w.timer) clearTimeout(w.timer);
      w.resolve(r);
    }
  }

  #fail(err: KevyError): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#closeErr = err;
    const pending = this.#waiters;
    this.#waiters = [];
    for (const w of pending) {
      if (w.timer) clearTimeout(w.timer);
      w.reject(err);
    }
    this.#sock.destroy();
  }

  #nextReply(timeoutMs: number | null): Promise<Reply> {
    if (this.#closed) return Promise.reject(this.#closeErr ?? new ClosedError());
    return new Promise<Reply>((resolve, reject) => {
      const w: Waiter = { resolve, reject };
      if (timeoutMs != null) {
        w.timer = setTimeout(() => {
          const i = this.#waiters.indexOf(w);
          if (i >= 0) this.#waiters.splice(i, 1);
          reject(new TimedOutError());
        }, timeoutMs);
      }
      this.#waiters.push(w);
      this.#drain();
    });
  }

  #send(wire: Uint8Array): void {
    if (this.#closed) throw this.#closeErr ?? new ClosedError();
    this.#sock.write(wire);
  }

  /** Send one command and await exactly one reply. */
  async request(argv: Uint8Array[]): Promise<Reply> {
    this.#send(encodeCommand(argv));
    return this.#nextReply(null);
  }

  /** Send a pre-encoded batch as one write and await n replies in order. */
  async pipelineRaw(raw: Uint8Array, n: number): Promise<Reply[]> {
    this.#send(raw);
    const out: Reply[] = [];
    for (let i = 0; i < n; i++) out.push(await this.#nextReply(null));
    return out;
  }

  /** Send a command without awaiting a reply (pub/sub producer side). */
  write(argv: Uint8Array[]): void {
    this.#send(encodeCommand(argv));
  }

  /** Await the next reply frame, honouring any read timeout (subscriber). */
  readReply(): Promise<Reply> {
    return this.#nextReply(this.#readTimeout);
  }

  /** Bound a blocking readReply (null clears it). */
  setReadTimeout(ms: number | null): void {
    this.#readTimeout = ms;
  }

  /** Issue a SELECT n round-trip; a -ERR reply fails the connect. */
  async selectDB(db: number): Promise<void> {
    const enc = new TextEncoder();
    const r = await this.request([enc.encode("SELECT"), enc.encode(String(db))]);
    if (r.kind === "error") {
      throw new ProtocolError(`SELECT ${db} rejected: ${new TextDecoder().decode(r.bytes)}`);
    }
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#closeErr = new ClosedError();
    this.#sock.destroy();
  }
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}
