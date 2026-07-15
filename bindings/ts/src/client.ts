// The unified client (contract §1). connect(url) routes to the embedded
// engine (mem:// / file://) or a remote RESP server (kevy:// / redis:// /
// tcp://); the same typed methods run against either. Async-by-default —
// every command family is a Promise method — with a synchronous embedded
// escape hatch on `.sync` (contract §1.4 / §7). Both faces bind the SAME
// command specs, so they cannot disagree on results.

import { replyText, toBytes, unexpectedReply, type Data, type Reply } from "./resp.ts";
import { InvalidInputError, UnsupportedError } from "./errors.ts";
import { parseConnectURL } from "./url.ts";
import { RespConn } from "./transport.ts";
import { EmbeddedDb, resolveStore } from "./embedded.ts";
import { EmbeddedBackend, RemoteBackend, type Backend } from "./backend.ts";
import { replyError } from "./reply.ts";
import {
  coreSpecs,
  hashSpecs,
  listSpecs,
  setSpecs,
  zsetSpecs,
  zalgebraSpecs,
  hashttlSpecs,
  type Spec,
} from "./specs.ts";
import { idxSpecs } from "./index_ops.ts";
import {
  blpopAsync,
  bzpopminAsync,
  blpopSync,
  bzpopminSync,
  type KV,
  type ZPopHit,
} from "./blocking.ts";
import {
  feedShards as feedShardsOp,
  feedTail as feedTailOp,
  feedRead as feedReadOp,
  feedShardsSync,
  feedTailSync,
  feedReadSync,
  type FeedBatch,
  type FeedCursor,
} from "./feed.ts";
import { Transaction, simpleOK } from "./transaction.ts";
import { runPipeline, type PipelineBuf } from "./pipeline.ts";

const ALL_SPECS = {
  ...coreSpecs,
  ...hashSpecs,
  ...listSpecs,
  ...setSpecs,
  ...zsetSpecs,
  ...zalgebraSpecs,
  ...hashttlSpecs,
  ...idxSpecs,
};
type AllSpecs = typeof ALL_SPECS;

type AsyncFn<S> = S extends Spec<infer A, infer T> ? (...a: A) => Promise<T> : never;
type SyncFn<S> = S extends Spec<infer A, infer T> ? (...a: A) => T : never;
type AsyncSpecFace<M> = { [K in keyof M]: AsyncFn<M[K]> };
type SyncSpecFace<M> = { [K in keyof M]: SyncFn<M[K]> };

/** The synchronous, embedded-only face (contract §1.4 / §7). Remote backends
 * throw Unsupported; remote-only commands (IDX.*, MULTI, pipeline) throw. */
export type SyncClient = SyncSpecFace<AllSpecs> & {
  readonly embedded: boolean;
  /** Raw-argv escape hatch — returns the Reply (a -ERR is data, not a throw). */
  do(...argv: Data[]): Reply;
  cmd(...argv: Data[]): Reply;
  ping(): void;
  blpop(keys: Data[], timeoutMs?: number | null): KV | null;
  brpop(keys: Data[], timeoutMs?: number | null): KV | null;
  bzpopmin(keys: Data[], timeoutMs?: number | null): ZPopHit | null;
  feedShards(): number;
  feedTail(shard: number): FeedCursor;
  feedRead(shard: number): FeedBatch;
};

/** One open connection to a kevy backend (contract §1). */
export type Client = AsyncSpecFace<AllSpecs> & {
  readonly url: string;
  readonly embedded: boolean;
  close(): void;
  /** Raw-argv escape hatch — returns the Reply (a -ERR is data, not a throw). */
  do(...argv: Data[]): Promise<Reply>;
  cmd(...argv: Data[]): Promise<Reply>;
  ping(): Promise<void>;
  blpop(keys: Data[], timeoutMs?: number | null): Promise<KV | null>;
  brpop(keys: Data[], timeoutMs?: number | null): Promise<KV | null>;
  bzpopmin(keys: Data[], timeoutMs?: number | null): Promise<ZPopHit | null>;
  feedShards(): Promise<number>;
  feedTail(shard: number): Promise<FeedCursor>;
  feedRead(shard: number, generation: number, offset: number, count?: number | null, ...prefixes: Data[]): Promise<FeedBatch>;
  watch(...keys: Data[]): Promise<void>;
  unwatch(): Promise<void>;
  multi(): Promise<Transaction>;
  pipeline(build: (p: PipelineBuf) => void): Promise<Reply[]>;
  /** The synchronous embedded-only face. */
  readonly sync: SyncClient;
};

type SpecMap = Record<string, Spec<unknown[], unknown>>;

/** Open a backend chosen by URL scheme (contract §1.1). */
export async function connect(url: string): Promise<Client> {
  const t = parseConnectURL(url);
  if (t.kind === "remote") {
    const conn = await RespConn.dial(t.host, t.port);
    if (t.db != null) {
      try {
        await conn.selectDB(t.db);
      } catch (e) {
        conn.close();
        throw e;
      }
    }
    return makeClient(new RemoteBackend(conn), url);
  }
  const [db, key] = resolveStore(t);
  return makeClient(new EmbeddedBackend(db, key), url);
}

/** Open an embedded backend synchronously (contract §1.4). Remote URLs need
 * async connect() — a dial cannot be synchronous in Node/Bun. */
export function connectSync(url: string): Client {
  const t = parseConnectURL(url);
  if (t.kind === "remote") {
    throw new UnsupportedError("connectSync is embedded-only; remote backends need the async connect()");
  }
  const [db, key] = resolveStore(t);
  return makeClient(new EmbeddedBackend(db, key), url);
}

function makeClient(be: Backend, url: string): Client {
  const extra = {
    url,
    embedded: be.isEmbedded,
    close: () => be.close(),
    do: (...argv: Data[]) => rawDo(be, argv),
    cmd: (...argv: Data[]) => rawDo(be, argv),
    ping: () => ping(be),
    blpop: (keys: Data[], t: number | null = null) => blpopAsync(be, "LPOP", keys, t),
    brpop: (keys: Data[], t: number | null = null) => blpopAsync(be, "RPOP", keys, t),
    bzpopmin: (keys: Data[], t: number | null = null) => bzpopminAsync(be, keys, t),
    feedShards: () => feedShardsOp(be),
    feedTail: (shard: number) => feedTailOp(be, shard),
    feedRead: (shard: number, generation: number, offset: number, count: number | null = null, ...prefixes: Data[]) =>
      feedReadOp(be, shard, generation, offset, count, prefixes),
    watch: (...keys: Data[]) => watch(be, keys),
    unwatch: () => unwatch(be),
    multi: () => multi(be),
    pipeline: async (build: (p: PipelineBuf) => void) => runPipeline(requireRemote(be, "pipeline"), build),
    sync: makeSyncClient(be),
  };
  return Object.assign(bindAsync(be), extra) as Client;
}

function makeSyncClient(be: Backend): SyncClient {
  const extra = {
    embedded: be.isEmbedded,
    do: (...argv: Data[]) => rawDoSync(be, argv),
    cmd: (...argv: Data[]) => rawDoSync(be, argv),
    ping: () => {
      if (!be.isEmbedded) be.execSync([toBytes("PING")]); // throws Unsupported on remote
    },
    blpop: (keys: Data[], t: number | null = null) => blpopSync(be, "LPOP", keys, t),
    brpop: (keys: Data[], t: number | null = null) => blpopSync(be, "RPOP", keys, t),
    bzpopmin: (keys: Data[], t: number | null = null) => bzpopminSync(be, keys, t),
    feedShards: () => feedShardsSync(be),
    feedTail: (shard: number) => feedTailSync(be, shard),
    feedRead: (shard: number) => feedReadSync(be, shard),
  };
  return Object.assign(bindSync(be), extra) as SyncClient;
}

function bindAsync(be: Backend): AsyncSpecFace<AllSpecs> {
  const specs = ALL_SPECS as SpecMap;
  const o: Record<string, (...a: unknown[]) => Promise<unknown>> = {};
  for (const name of Object.keys(specs)) {
    const s = specs[name]!;
    o[name] = async (...args) => {
      if (s.remoteOnly && be.isEmbedded) {
        throw new UnsupportedError(`${name} is remote-only; on embedded use cmd() or the store's typed API`);
      }
      return s.parse(await be.exec(s.build(...args)));
    };
  }
  return o as AsyncSpecFace<AllSpecs>;
}

function bindSync(be: Backend): SyncSpecFace<AllSpecs> {
  const specs = ALL_SPECS as SpecMap;
  const o: Record<string, (...a: unknown[]) => unknown> = {};
  for (const name of Object.keys(specs)) {
    const s = specs[name]!;
    o[name] = (...args) => {
      if (s.remoteOnly) throw new UnsupportedError(`${name} is remote-only and the sync face is embedded-only`);
      return s.parse(be.execSync(s.build(...args)));
    };
  }
  return o as SyncSpecFace<AllSpecs>;
}

async function rawDo(be: Backend, argv: Data[]): Promise<Reply> {
  if (argv.length === 0) throw new InvalidInputError("do needs at least a verb");
  return be.exec(argv.map(toBytes));
}
function rawDoSync(be: Backend, argv: Data[]): Reply {
  if (argv.length === 0) throw new InvalidInputError("do needs at least a verb");
  return be.execSync(argv.map(toBytes));
}

async function ping(be: Backend): Promise<void> {
  if (be.isEmbedded) return;
  const r = await be.exec([toBytes("PING")]);
  if (r.kind === "simple" && replyText(r) === "PONG") return;
  if (r.kind === "error") replyError(r);
  throw unexpectedReply(r);
}

function requireRemote(be: Backend, feature: string): RespConn {
  if (be.isEmbedded) {
    throw new UnsupportedError(`${feature} is remote-only; on the embedded backend use the cmd() raw path`);
  }
  return be.remoteConn();
}

async function watch(be: Backend, keys: Data[]): Promise<void> {
  if (keys.length === 0) throw new InvalidInputError("WATCH needs at least one key");
  await simpleOK(requireRemote(be, "WATCH"), [toBytes("WATCH"), ...keys.map(toBytes)]);
}
async function unwatch(be: Backend): Promise<void> {
  await simpleOK(requireRemote(be, "UNWATCH"), [toBytes("UNWATCH")]);
}
async function multi(be: Backend): Promise<Transaction> {
  const conn = requireRemote(be, "MULTI/EXEC");
  await simpleOK(conn, [toBytes("MULTI")]);
  return new Transaction(conn);
}

export { EmbeddedDb };
