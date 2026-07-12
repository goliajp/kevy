// @goliajp/kevy — hand-written ES-module loader for the kevy WebAssembly
// module. No dependencies, no binding generator: this file owns the
// TypedArray boundary, UTF-8 codecs, the persistence pump (OPFS worker or
// IndexedDB), and the cross-tab pub/sub bridge (BroadcastChannel).
//
// Storage backends: OPFS (synchronous access handle inside a worker) is
// the primary; IndexedDB is the fallback where OPFS sync handles are
// unavailable. localStorage is deliberately NOT a backend: a ~5 MB quota,
// a synchronous API that blocks the main thread on every write, and
// UTF-16 string-only storage make it the wrong tool for an append log.

const ABI_VERSION = 1;

const te = new TextEncoder();
const td = new TextDecoder();

/**
 * Open a kevy instance.
 *
 * @param {object} [options]
 * @param {*} [options.wasm] Module source: URL/string, ArrayBuffer,
 *   Uint8Array, Response, or a compiled WebAssembly.Module. Defaults to
 *   `kevy.wasm` next to this file.
 * @param {false|object} [options.persist] Durability. `false`/omitted =
 *   pure in-memory. `{ name, backend }` persists the write log under
 *   `name` (default "kevy") via `backend`: "auto" (default; OPFS with
 *   IndexedDB fallback), "opfs", or "idb".
 * @param {boolean} [options.broadcast] Cross-tab pub/sub bridge over
 *   BroadcastChannel (default true).
 * @param {string} [options.name] Instance name; scopes both the storage
 *   file and the broadcast channel. Defaults to persist.name or "kevy".
 * @param {number} [options.tickMs] TTL sweep + event poll cadence in ms
 *   (default 100; 0 disables the timer — call `tick()` yourself).
 * @returns {Promise<Kevy>}
 */
export async function open(options = {}) {
  const persist = options.persist ?? false;
  // Storage bring-up (worker spawn, OPFS handle, IndexedDB open) runs
  // concurrently with wasm instantiation — both are independent I/O.
  const backendPromise = persist
    ? openBackend(persist.backend ?? "auto", persist.name ?? options.name ?? "kevy")
    : null;
  try {
    const exports = await instantiate(options.wasm);
    if (exports.kevy_abi_version() !== ABI_VERSION) {
      throw new Error(
        `kevy: ABI version mismatch (module ${exports.kevy_abi_version()}, loader ${ABI_VERSION})`,
      );
    }
    const kevy = new Kevy(exports, options);
    await kevy._init(options, backendPromise);
    return kevy;
  } catch (err) {
    if (backendPromise) backendPromise.then((b) => b.close()).catch(() => {});
    throw err;
  }
}

async function instantiate(source) {
  const src = source ?? new URL("./kevy.wasm", import.meta.url);
  if (src instanceof WebAssembly.Module) {
    return (await WebAssembly.instantiate(src, {})).exports;
  }
  if (src instanceof ArrayBuffer || ArrayBuffer.isView(src)) {
    return (await WebAssembly.instantiate(src, {})).instance.exports;
  }
  const resp = src instanceof Response ? src : await fetch(src);
  if (typeof WebAssembly.instantiateStreaming === "function") {
    try {
      return (await WebAssembly.instantiateStreaming(resp, {})).instance.exports;
    } catch {
      // Server may not send application/wasm — fall through to buffering.
    }
  }
  const bytes = await resp.arrayBuffer();
  return (await WebAssembly.instantiate(bytes, {})).instance.exports;
}

/** One open kevy store. Construct via {@link open}. */
export class Kevy {
  #e; // wasm exports
  #h = 0; // instance handle
  #subs = new Map(); // subId -> { cb, dispose-safe }
  #bc = null; // BroadcastChannel bridge
  #senderId; // random id for cross-tab self-loop suppression
  #seq = 0;
  #backend = null; // persistence backend (OPFS worker or IndexedDB)
  #writeChain = Promise.resolve(); // serialized backend writes
  #pumpScheduled = false;
  #appendedSinceCompact = 0;
  #lastImageSize = 0;
  #timer = null;
  #closed = false;
  // Hot-path staging: one grow-only scratch allocation reused by every
  // KV call (no per-op alloc/free), a cached linear-memory view
  // (rebuilt only when the memory grows), and a same-millisecond clock
  // dedupe (feeding an identical timestamp is a no-op by definition).
  #scratchPtr = 0;
  #scratchLen = 0;
  #memView = null;
  #lastClockMs = -1;

  constructor(exports, options) {
    this.#e = exports;
    this.#senderId =
      Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 10);
    this._name = options.name ?? options.persist?.name ?? "kevy";
  }

  /** @private Second-stage open: storage replay, bridge, timers. */
  async _init(options, backendPromise) {
    this.#clock();
    this.#h = this.#e.kevy_open(backendPromise ? 1 : 0);
    if (this.#h === 0) throw new Error("kevy: open failed");

    if (backendPromise) {
      this.#backend = await backendPromise;
      const log = await this.#backend.load();
      if (log.byteLength > 0) {
        this.#clock();
        this.#feedLog(log);
        await this.#maybeCompact(log.byteLength);
      }
    }

    if (options.broadcast !== false && typeof BroadcastChannel === "function") {
      // One channel per instance name, protocol-versioned so a future
      // frame format bump cannot be misparsed by older tabs. Topic
      // filtering happens in-engine; frames stay flat (one Uint8Array
      // payload) to keep the structured-clone cost linear in the bytes.
      this.#bc = new BroadcastChannel(`kevy-wasm:${this._name}:pubsub:v1`);
      this.#bc.onmessage = (ev) => this.#onBroadcast(ev.data);
    }

    const tickMs = options.tickMs ?? 100;
    if (tickMs > 0) {
      this.#timer = setInterval(() => {
        if (this.#closed) return;
        this.tick();
      }, tickMs);
    }
  }

  // ---- byte boundary ----------------------------------------------------

  #clock() {
    const now = Date.now();
    if (now !== this.#lastClockMs) {
      this.#lastClockMs = now;
      this.#e.kevy_set_clock(now);
    }
  }

  #bytes(x) {
    if (x instanceof Uint8Array) return x;
    if (x instanceof ArrayBuffer) return new Uint8Array(x);
    return te.encode(String(x));
  }

  /** Cached whole-memory view; rebuilt after a memory growth. */
  #mem() {
    if (this.#memView === null || this.#memView.buffer !== this.#e.memory.buffer) {
      this.#memView = new Uint8Array(this.#e.memory.buffer);
    }
    return this.#memView;
  }

  /** Worst-case staged size of a value (strings encode to ≤ 3×). */
  #cap(x) {
    return typeof x === "string" ? x.length * 3 : x.byteLength;
  }

  #ensureScratch(n) {
    if (this.#scratchLen < n) {
      if (this.#scratchPtr) {
        this.#e.kevy_free(this.#scratchPtr, this.#scratchLen);
      }
      this.#scratchLen = Math.max(256, 1 << (32 - Math.clz32(n - 1)));
      this.#scratchPtr = this.#e.kevy_alloc(this.#scratchLen);
      this.#memView = null; // the alloc may have grown memory
    }
    return this.#scratchPtr;
  }

  /** Encode `x` into linear memory at `off`; returns the byte length. */
  #encodeAt(mem, x, off) {
    if (typeof x === "string") {
      return te.encodeInto(x, mem.subarray(off, off + x.length * 3)).written;
    }
    const b = x instanceof Uint8Array ? x : new Uint8Array(x);
    mem.set(b, off);
    return b.byteLength;
  }

  /** Stage one argument in the scratch buffer. Returns [ptr, len]. */
  #stage1(a) {
    const ptr = this.#ensureScratch(this.#cap(a));
    return [ptr, this.#encodeAt(this.#mem(), a, ptr)];
  }

  /** Stage two arguments in the scratch buffer. */
  #stage2(a, b) {
    const capA = this.#cap(a);
    const ptr = this.#ensureScratch(capA + this.#cap(b));
    const mem = this.#mem();
    const la = this.#encodeAt(mem, a, ptr);
    const lb = this.#encodeAt(mem, b, ptr + capA);
    return [ptr, la, ptr + capA, lb];
  }


  /** Copy the instance result buffer out of linear memory. */
  #out() {
    const len = this.#e.kevy_out_len(this.#h);
    if (len === 0) return new Uint8Array(0);
    const ptr = this.#e.kevy_out_ptr(this.#h);
    return new Uint8Array(this.#e.memory.buffer, ptr, len).slice();
  }

  #check(status) {
    if (status === -1) throw new Error(`kevy: ${td.decode(this.#out())}`);
    if (status === -2) throw new Error("kevy: instance is closed");
    return status;
  }

  // ---- KV + TTL -----------------------------------------------------------

  /** SET. `value` is a string or Uint8Array; `ttlMs` sets an expiry. */
  set(key, value, opts) {
    this.#clock();
    const [kp, kl, vp, vl] = this.#stage2(key, value);
    const ttlMs = opts === undefined ? 0 : opts.ttlMs;
    this.#check(
      ttlMs > 0
        ? this.#e.kevy_set_ttl(this.#h, kp, kl, vp, vl, ttlMs)
        : this.#e.kevy_set(this.#h, kp, kl, vp, vl),
    );
    this.#dirty();
  }

  /** GET as raw bytes; `undefined` when absent or expired. */
  get(key) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    const hit = this.#check(this.#e.kevy_get(this.#h, p, l));
    return hit === 1 ? this.#out() : undefined;
  }

  /** GET decoded as UTF-8 text; `undefined` when absent or expired. */
  getText(key) {
    const v = this.get(key);
    return v === undefined ? undefined : td.decode(v);
  }

  /**
   * MGET — read many keys in a single crossing into wasm.
   *
   * A per-key `get()` pays the boundary cost every call: encode the key
   * into linear memory, call, copy the value back. On small values that
   * crossing dominates the lookup. `mget` pays it once for the whole
   * batch, which is what makes wasm KV competitive on read-heavy work.
   *
   * Returns an array parallel to `keys`: a `Uint8Array` per hit,
   * `undefined` per miss.
   */
  mget(keys) {
    if (!keys.length) return [];
    this.#clock();
    // Stage every key into one scratch buffer: [len u32][key bytes]…
    let cap = 0;
    for (const k of keys) cap += 4 + this.#cap(k);
    const ptr = this.#ensureScratch(cap);
    const mem = this.#mem();
    const dv = new DataView(mem.buffer);
    let off = ptr;
    for (const k of keys) {
      const body = off + 4;
      const len = this.#encodeAt(mem, k, body);
      dv.setUint32(off, len, true);
      off = body + len;
    }
    const n = this.#check(this.#e.kevy_mget(this.#h, ptr, off - ptr, keys.length));
    const buf = this.#out();
    const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    const out = [];
    let i = 0;
    for (let j = 0; j < n; j++) {
      const len = view.getUint32(i, true);
      i += 4;
      if (len === 0xffffffff) { out.push(undefined); continue; } // miss sentinel
      out.push(buf.slice(i, i + len));
      i += len;
    }
    return out;
  }

  /** MGET decoded as UTF-8 text; `undefined` per miss. */
  mgetText(keys) {
    return this.mget(keys).map((v) => (v === undefined ? undefined : td.decode(v)));
  }

  /** DEL. Returns true if the key existed. */
  del(key) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    const n = this.#check(this.#e.kevy_del(this.#h, p, l));
    this.#dirty();
    return n > 0;
  }

  /** EXISTS. */
  exists(key) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    return this.#check(this.#e.kevy_exists(this.#h, p, l)) > 0;
  }

  /** PEXPIRE — set/replace the TTL. Returns true if the key exists. */
  expire(key, ttlMs) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    const r = this.#check(this.#e.kevy_expire(this.#h, p, l, ttlMs));
    this.#dirty();
    return r === 1;
  }

  /** PERSIST — clear the TTL. Returns true if a TTL was removed. */
  persist(key) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    const r = this.#check(this.#e.kevy_persist(this.#h, p, l));
    this.#dirty();
    return r === 1;
  }

  /** PTTL in ms; -1 = no TTL, -2 = no key. */
  pttl(key) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    const r = this.#e.kevy_pttl(this.#h, p, l);
    if (Number.isNaN(r)) throw new Error("kevy: instance is closed");
    return r;
  }

  /** INCRBY (delta may be negative). Returns the new value. */
  incrby(key, delta = 1) {
    this.#clock();
    const [p, l] = this.#stage1(key);
    this.#check(this.#e.kevy_incrby(this.#h, p, l, delta));
    this.#dirty();
    return Number(td.decode(this.#out()));
  }

  /** DBSIZE — live key count. */
  dbsize() {
    const r = this.#e.kevy_dbsize(this.#h);
    if (Number.isNaN(r)) throw new Error("kevy: instance is closed");
    return r;
  }

  /** FLUSHALL — wipe the keyspace. */
  flushall() {
    this.#clock();
    this.#check(this.#e.kevy_flushall(this.#h));
    this.#dirty();
  }

  /** KEYS matching a Redis glob (default: all), up to `limit` (0 = all). */
  keys(pattern = "", limit = 0) {
    this.#clock();
    const [p, l] = this.#stage1(pattern);
    const n = this.#check(this.#e.kevy_keys(this.#h, p, l, limit));
    const buf = this.#out();
    const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    const out = [];
    let i = 0;
    for (let k = 0; k < n; k++) {
      const len = view.getUint32(i, true);
      i += 4;
      out.push(td.decode(buf.subarray(i, i + len)));
      i += len;
    }
    return out;
  }

  /** One manual TTL sweep + event poll. Returns expired-key count. */
  tick() {
    this.#clock();
    const expired = this.#check(this.#e.kevy_tick(this.#h));
    this.#drainEvents();
    this.#dirty(); // expiry does not write frames, but pump stays cheap when idle
    return expired;
  }

  // ---- pub/sub ------------------------------------------------------------

  /**
   * SUBSCRIBE. `cb(payload: Uint8Array, channel: string)` fires for every
   * message, including messages published by other tabs when the
   * broadcast bridge is on. Returns an unsubscribe function.
   */
  subscribe(channel, cb) {
    const [p, l] = this.#stage1(channel);
    const id = this.#e.kevy_subscribe(this.#h, p, l);
    if (id === 0) throw new Error("kevy: instance is closed");
    this.#subs.set(id, cb);
    return () => {
      this.#subs.delete(id);
      this.#e.kevy_unsubscribe(this.#h, id);
    };
  }

  /** PSUBSCRIBE with a Redis glob. `cb(payload, channel, pattern)`. */
  psubscribe(pattern, cb) {
    const [p, l] = this.#stage1(pattern);
    const id = this.#e.kevy_psubscribe(this.#h, p, l);
    if (id === 0) throw new Error("kevy: instance is closed");
    this.#subs.set(id, cb);
    return () => {
      this.#subs.delete(id);
      this.#e.kevy_unsubscribe(this.#h, id);
    };
  }

  /**
   * PUBLISH to local subscribers and (bridge on) every other same-origin
   * tab. Returns the local receiver count. Cross-tab delivery is
   * at-most-once with no backlog: only currently-open tabs receive the
   * message, exactly like a server-side pub/sub client that is not
   * connected at publish time.
   */
  publish(channel, payload) {
    const [cp, cl, pp, pl] = this.#stage2(channel, payload);
    const n = this.#check(this.#e.kevy_publish(this.#h, cp, cl, pp, pl));
    if (this.#bc) {
      // Flat frame, one binary payload; sender id suppresses the loop
      // through our own engine when the frame comes back to this page.
      this.#bc.postMessage({
        v: 1,
        sender: this.#senderId,
        seq: this.#seq++,
        channel:
          typeof channel === "string" ? channel : td.decode(this.#bytes(channel)),
        buf: this.#mem().slice(pp, pp + pl),
      });
    }
    queueMicrotask(() => this.#drainEvents());
    return n;
  }

  #onBroadcast(frame) {
    if (this.#closed || !frame || frame.v !== 1) return;
    if (frame.sender === this.#senderId) return;
    const [cp, cl, pp, pl] = this.#stage2(frame.channel, frame.buf);
    this.#e.kevy_publish(this.#h, cp, cl, pp, pl);
    this.#drainEvents();
  }

  #drainEvents() {
    if (this.#closed) return;
    const n = this.#e.kevy_poll_events(this.#h);
    if (n <= 0) return;
    const buf = this.#out();
    const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    let i = 0;
    for (let k = 0; k < n; k++) {
      const kind = buf[i];
      const sub = view.getUint32(i + 1, true);
      i += 5;
      const segs = [];
      for (let s = 0; s < 3; s++) {
        const len = view.getUint32(i, true);
        i += 4;
        segs.push(buf.subarray(i, i + len));
        i += len;
      }
      const cb = this.#subs.get(sub);
      if (!cb) continue;
      const [pattern, channel, payload] = segs;
      if (kind === 1) cb(payload.slice(), td.decode(channel));
      else cb(payload.slice(), td.decode(channel), td.decode(pattern));
    }
  }

  // ---- persistence pump -----------------------------------------------------

  #dirty() {
    if (!this.#backend || this.#pumpScheduled || this.#closed) return;
    this.#pumpScheduled = true;
    queueMicrotask(() => this.#pump());
  }

  #pump() {
    this.#pumpScheduled = false;
    if (!this.#backend || this.#closed) return;
    const len = this.#e.kevy_aof_frames_out(this.#h);
    if (len <= 0) return;
    const frames = this.#out();
    this.#appendedSinceCompact += frames.byteLength;
    this.#writeChain = this.#writeChain.then(() => this.#backend.append(frames));
    // Keep storage proportional to the live keyspace, not the write
    // history: rewrite once the appended log outgrows the last image.
    if (
      this.#appendedSinceCompact >
      Math.max(512 * 1024, 4 * this.#lastImageSize)
    ) {
      this.compact();
    }
  }

  #feedLog(log) {
    const CHUNK = 1 << 20;
    for (let off = 0; off < log.byteLength; off += CHUNK) {
      const chunk = log.subarray(off, Math.min(off + CHUNK, log.byteLength));
      const [p, l] = this.#stage1(chunk);
      const n = this.#e.kevy_aof_frame_in(this.#h, p, l);
      if (n === -1) {
        // Same contract as a native kevy replay: the intact prefix is
        // applied, the corrupt tail is dropped. The next compaction
        // rewrites storage from live state.
        console.warn(`kevy: ${td.decode(this.#out())}`);
        break;
      }
    }
  }

  async #maybeCompact(loadedBytes) {
    if (loadedBytes < 4096) return;
    const image = this.#dumpImage();
    if (image.byteLength < loadedBytes * 0.75) {
      this.#writeChain = this.#writeChain.then(() =>
        this.#backend.replace(image),
      );
      this.#appendedSinceCompact = 0;
      this.#lastImageSize = image.byteLength;
      await this.#writeChain;
    }
  }

  #dumpImage() {
    this.#clock();
    this.#check(this.#e.kevy_aof_dump(this.#h));
    return this.#out();
  }

  /**
   * Durability barrier: pump every pending write frame to storage and
   * resolve once the backend has flushed it.
   */
  async flush() {
    if (!this.#backend) return;
    this.#pump();
    await this.#writeChain;
  }

  /**
   * Rewrite storage as a compacted image of the live keyspace (the
   * browser-side equivalent of an AOF rewrite). Runs automatically as
   * the log grows; call it manually before snapshots/exports if wanted.
   */
  async compact() {
    if (!this.#backend) return;
    const image = this.#dumpImage(); // subsumes any pending frames
    this.#appendedSinceCompact = 0;
    this.#lastImageSize = image.byteLength;
    this.#writeChain = this.#writeChain.then(() => this.#backend.replace(image));
    await this.#writeChain;
  }

  /** Flush, tear down the bridge/timer/backend, and free the instance. */
  async close() {
    if (this.#closed) return;
    if (this.#timer) clearInterval(this.#timer);
    await this.flush();
    this.#closed = true;
    if (this.#bc) this.#bc.close();
    if (this.#backend) await this.#backend.close();
    for (const id of this.#subs.keys()) this.#e.kevy_unsubscribe(this.#h, id);
    this.#subs.clear();
    if (this.#scratchPtr) {
      this.#e.kevy_free(this.#scratchPtr, this.#scratchLen);
      this.#scratchPtr = 0;
      this.#scratchLen = 0;
    }
    this.#e.kevy_close(this.#h);
  }
}

// ---- storage backends --------------------------------------------------

async function openBackend(kind, name) {
  if (kind === "opfs" || kind === "auto") {
    try {
      return await OpfsBackend.open(name);
    } catch (err) {
      if (kind === "opfs") throw err;
      // auto: fall through to IndexedDB
    }
  }
  return IdbBackend.open(name);
}

/**
 * OPFS backend: a dedicated worker owns a FileSystemSyncAccessHandle on
 * `kevy-wasm/<name>.aof` and serves load / append / replace requests.
 * Sync handles give ordered, flushable writes without touching the main
 * thread.
 */
class OpfsBackend {
  #worker;
  #next = 1;
  #pending = new Map();
  #loaded = null;

  static async open(name) {
    if (!navigator.storage?.getDirectory || typeof Worker !== "function") {
      throw new Error("kevy: OPFS unavailable");
    }
    const b = new OpfsBackend();
    b.#worker = new Worker(new URL("./kevy-opfs-worker.js", import.meta.url), {
      type: "module",
    });
    b.#worker.onmessage = (ev) => {
      const { id, ok, buf, err } = ev.data;
      const p = b.#pending.get(id);
      if (!p) return;
      b.#pending.delete(id);
      ok ? p.resolve(buf) : p.reject(new Error(`kevy: OPFS: ${err}`));
    };
    // The open reply carries the file content (one round trip for both).
    b.#loaded = (await b.#call("open", { name })) ?? new Uint8Array(0);
    return b;
  }

  #call(op, extra = {}, transfer = []) {
    const id = this.#next++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#worker.postMessage({ id, op, ...extra }, transfer);
    });
  }

  async load() {
    const data = this.#loaded ?? new Uint8Array(0);
    this.#loaded = null;
    return data;
  }
  async append(bytes) {
    await this.#call("append", { buf: bytes }, [bytes.buffer]);
  }
  async replace(bytes) {
    await this.#call("replace", { buf: bytes }, [bytes.buffer]);
  }
  async close() {
    await this.#call("close");
    this.#worker.terminate();
  }
}

/**
 * IndexedDB backend: the log is a sequence of byte chunks in an
 * auto-increment object store; load concatenates them in key order,
 * replace clears and writes one chunk.
 */
class IdbBackend {
  #db;

  static open(name) {
    return new Promise((resolve, reject) => {
      const req = indexedDB.open(`kevy-wasm:${name}`, 1);
      req.onupgradeneeded = () => req.result.createObjectStore("aof", { autoIncrement: true });
      req.onerror = () => reject(req.error);
      req.onsuccess = () => {
        const b = new IdbBackend();
        b.#db = req.result;
        resolve(b);
      };
    });
  }

  #tx(mode, run) {
    return new Promise((resolve, reject) => {
      const tx = this.#db.transaction("aof", mode);
      const result = run(tx.objectStore("aof"));
      tx.oncomplete = () => resolve(result);
      tx.onerror = () => reject(tx.error);
      tx.onabort = () => reject(tx.error);
    });
  }

  async load() {
    const chunks = [];
    await this.#tx("readonly", (store) => {
      store.openCursor().onsuccess = (ev) => {
        const cur = ev.target.result;
        if (cur) {
          chunks.push(cur.value);
          cur.continue();
        }
      };
    });
    const total = chunks.reduce((n, c) => n + c.byteLength, 0);
    const out = new Uint8Array(total);
    let off = 0;
    for (const c of chunks) {
      out.set(c instanceof Uint8Array ? c : new Uint8Array(c), off);
      off += c.byteLength;
    }
    return out;
  }

  append(bytes) {
    return this.#tx("readwrite", (store) => store.add(bytes));
  }

  replace(bytes) {
    return this.#tx("readwrite", (store) => {
      store.clear();
      store.add(bytes);
    });
  }

  async close() {
    this.#db.close();
  }
}
