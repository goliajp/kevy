// Types for @goliapkg/kevy-electron — hand-written, matching index.js
// (main process) and preload.cjs (renderer). See README.md.

/** Bytes in: strings are UTF-8 encoded; byte views pass through binary-safe. */
export type Bytes = string | Uint8Array;

/** A protocol error returned as a VALUE by cmd() (the engine saying no is
 *  data; the typed verbs throw instead). */
export declare class KevyError {
  constructor(message: string);
  readonly message: string;
}

/** A decoded RESP reply (contract §4.1), as cmd() yields it in the renderer. */
export type Reply = string | number | bigint | Uint8Array | null | KevyError | Reply[];

/** IPC channel names shared by the bridge and preload. */
export declare const CHANNELS: Readonly<Record<string, string>>;

// ── main process ──────────────────────────────────────────────────────────

/** Electron's ipcMain, injected so the bridge stays unit-testable. */
export interface IpcMainLike {
  handle(channel: string, listener: (event: unknown, ...args: any[]) => unknown): void;
  removeHandler?(channel: string): void;
}

/** The open Node-door store the bridge drives (a subset of @goliapkg/kevy-node's Db). */
export interface KevyStore {
  cmd(...argv: Bytes[]): Reply;
  get(key: Bytes): Uint8Array | null;
  set(key: Bytes, value: Bytes, opts?: { ttlMs?: number }): void;
  del(...keys: Bytes[]): number;
  incrby(key: Bytes, delta: number): number;
  expire(key: Bytes, ttlMs: number): boolean;
  pttl(key: Bytes): number;
  mget(...keys: Bytes[]): (Uint8Array | null)[];
  publish(channel: Bytes, payload: Bytes): number;
  subscribeRaw(channel: Bytes): unknown;
  psubscribeRaw(pattern: Bytes): unknown;
  close(): void;
}

export interface RegisterOptions {
  ipcMain: IpcMainLike;
  db: KevyStore;
  /** Pub/sub pump cadence in ms. Default 50. */
  tickMs?: number;
  /** Version string returned to renderers by window.kevy.version(). */
  version?: string;
}

export interface KevyBridge {
  /** Drain every live subscription once (the pump also runs on a timer). */
  pump(): void;
  /** Remove all handlers, stop the pump, close every subscription. */
  dispose(): void;
}

/** Register kevy's IPC handlers on an injected ipcMain over an open store. */
export declare function registerKevyHandlers(opts: RegisterOptions): KevyBridge;

export interface InstallOptions {
  ipcMain: IpcMainLike;
  /** Persistence directory; omit for a pure in-memory store. */
  dir?: string;
  /** Pub/sub pump cadence in ms. Default 50. */
  tickMs?: number;
  /** Adopt a pre-opened store instead of opening one (caller keeps ownership). */
  db?: KevyStore;
}

export interface KevyMain {
  /** The open store, for main-process use alongside the renderers. */
  db: KevyStore;
  /** Tear down: remove handlers, stop the pump, close the store (unless adopted). */
  dispose(): void;
}

/** Open a kevy store and expose it to renderers over IPC. */
export declare function installKevyMain(opts: InstallOptions): Promise<KevyMain>;

// ── renderer (window.kevy, from the preload) ────────────────────────────────

/** Message callback: (payload bytes, channel text). */
export type MessageCallback = (payload: Uint8Array, channel: string) => void;
/** Pattern-message callback: (payload bytes, channel text, pattern text). */
export type PatternCallback = (payload: Uint8Array, channel: string, pattern: string) => void;
/** Awaited result of subscribe/psubscribe: call it to unsubscribe. */
export type Unsubscribe = () => Promise<void>;

/** The async kevy API exposed on `window.kevy` by the preload. */
export interface KevyClient {
  /** Every verb; a protocol error resolves as a KevyError VALUE. */
  cmd(...argv: Bytes[]): Promise<Reply>;
  get(key: Bytes): Promise<Uint8Array | null>;
  getText(key: Bytes): Promise<string | undefined>;
  set(key: Bytes, value: Bytes, opts?: { ttlMs?: number }): Promise<void>;
  del(...keys: Bytes[]): Promise<number>;
  incrby(key: Bytes, delta?: number): Promise<number>;
  expire(key: Bytes, ttlMs: number): Promise<boolean>;
  ttl(key: Bytes): Promise<number>;
  mget(...keys: Bytes[]): Promise<(Uint8Array | null)[]>;
  publish(channel: Bytes, payload: Bytes): Promise<number>;
  version(): Promise<string>;
  subscribe(channel: Bytes, cb: MessageCallback): Promise<Unsubscribe>;
  psubscribe(pattern: Bytes, cb: PatternCallback): Promise<Unsubscribe>;
}

declare global {
  interface Window {
    kevy: KevyClient;
  }
}
