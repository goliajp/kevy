// Command specs (contract §3.1–§3.7). Each spec is a pure (build, parse)
// pair: build turns typed args into argv; parse turns the Reply into the
// typed result. Both the async and the sync face bind the SAME specs, so
// the two faces cannot disagree (contract §1.4). Backend-conditional
// families (ping, blocking pops, feed) are hand-written in client.ts; the
// spec tables here are the plain request→reply commands.

import { toBytes, type Data, type Reply } from "./resp.ts";
import { InvalidInputError } from "./errors.ts";
import { aggregateTag, condKeyword, type HExpireCond, type ZAggregate } from "./enums.ts";
import {
  asBool,
  asBulks,
  asCount,
  asInt,
  asIntList,
  asMget,
  asOK,
  asOptBulk,
  asOptDouble,
  asPopList,
  asStr,
  av,
  floatStr,
} from "./reply.ts";

/** A command spec: a build/parse pair, optionally remote-only (§3.8). */
export interface Spec<A extends unknown[], T> {
  build: (...a: A) => Uint8Array[];
  parse: (r: Reply) => T;
  remoteOnly: boolean;
}

export function spec<A extends unknown[], T>(
  build: (...a: A) => Uint8Array[],
  parse: (r: Reply) => T,
  remoteOnly = false,
): Spec<A, T> {
  return { build, parse, remoteOnly };
}

/** A (score, member) pair for ZADD. */
export interface ZMember {
  score: number;
  member: Data;
}

function msArg(ttlMs: number): string {
  return String(ttlMs < 0 ? 0 : Math.floor(ttlMs));
}

function even(parts: Data[], what: string): Data[] {
  if (parts.length % 2 !== 0) throw new InvalidInputError(`${what} needs an even number of arguments`);
  return parts;
}

function nonEmpty<T>(items: T[], what: string): T[] {
  if (items.length === 0) throw new InvalidInputError(what);
  return items;
}

// --- core string / generic key (§3.1) -----------------------------------

export const coreSpecs = {
  set: spec((key: Data, value: Data) => av("SET", key, value), asOK),
  get: spec((key: Data) => av("GET", key), asOptBulk),
  del: spec((...keys: Data[]) => av("DEL", ...keys), asCount),
  exists: spec((...keys: Data[]) => av("EXISTS", ...keys), asCount),
  incr: spec((key: Data) => av("INCR", key), asInt),
  incrBy: spec((key: Data, delta: number) => av("INCRBY", key, String(delta)), asInt),
  expire: spec((key: Data, ttlMs: number) => av("PEXPIRE", key, msArg(ttlMs)), asBool),
  persist: spec((key: Data) => av("PERSIST", key), asBool),
  ttlMs: spec((key: Data) => av("PTTL", key), asInt),
  typeOf: spec((key: Data) => av("TYPE", key), asStr),
  dbsize: spec(() => av("DBSIZE"), asCount),
  flushall: spec(() => av("FLUSHALL"), asOK),
  setWithTtl: spec((key: Data, value: Data, ttlMs: number) => av("SET", key, value, "PX", msArg(ttlMs)), asOK),
  mget: spec((...keys: Data[]) => av("MGET", ...keys), asMget),
  mset: spec((...pairs: Data[]) => av("MSET", ...even(pairs, "mset")), asOK),
  publish: spec((channel: Data, message: Data) => av("PUBLISH", channel, message), asCount),
};

// --- hash (§3.2) --------------------------------------------------------

export const hashSpecs = {
  hset: spec((key: Data, ...pairs: Data[]) => av("HSET", key, ...even(pairs, "hset")), asCount),
  hget: spec((key: Data, field: Data) => av("HGET", key, field), asOptBulk),
  hdel: spec((key: Data, ...fields: Data[]) => av("HDEL", key, ...fields), asCount),
  hlen: spec((key: Data) => av("HLEN", key), asCount),
  hgetall: spec((key: Data) => av("HGETALL", key), asBulks),
  hkeys: spec((key: Data) => av("HKEYS", key), asBulks),
  hvals: spec((key: Data) => av("HVALS", key), asBulks),
};

// --- list (§3.3) --------------------------------------------------------

export const listSpecs = {
  lpush: spec((key: Data, ...values: Data[]) => av("LPUSH", key, ...values), asCount),
  rpush: spec((key: Data, ...values: Data[]) => av("RPUSH", key, ...values), asCount),
  lpop: spec((key: Data, count: number) => av("LPOP", key, String(count)), asPopList),
  rpop: spec((key: Data, count: number) => av("RPOP", key, String(count)), asPopList),
  llen: spec((key: Data) => av("LLEN", key), asCount),
  lrange: spec((key: Data, start: number, stop: number) => av("LRANGE", key, String(start), String(stop)), asBulks),
};

// --- set (§3.4) ---------------------------------------------------------

export const setSpecs = {
  sadd: spec((key: Data, ...members: Data[]) => av("SADD", key, ...members), asCount),
  srem: spec((key: Data, ...members: Data[]) => av("SREM", key, ...members), asCount),
  smembers: spec((key: Data) => av("SMEMBERS", key), asBulks),
  scard: spec((key: Data) => av("SCARD", key), asCount),
  sismember: spec((key: Data, member: Data) => av("SISMEMBER", key, member), asBool),
  sinter: spec((...keys: Data[]) => av("SINTER", ...keys), asBulks),
  sunion: spec((...keys: Data[]) => av("SUNION", ...keys), asBulks),
  sdiff: spec((...keys: Data[]) => av("SDIFF", ...keys), asBulks),
};

// --- sorted set (§3.5) --------------------------------------------------

function zaddArgv(key: Data, members: ZMember[]): Uint8Array[] {
  const parts: Uint8Array[] = [toBytes("ZADD"), toBytes(key)];
  for (const m of members) {
    parts.push(toBytes(floatStr(m.score)), toBytes(m.member));
  }
  return parts;
}

export const zsetSpecs = {
  zadd: spec((key: Data, ...members: ZMember[]) => zaddArgv(key, members), asCount),
  zrem: spec((key: Data, ...members: Data[]) => av("ZREM", key, ...members), asCount),
  zscore: spec((key: Data, member: Data) => av("ZSCORE", key, member), asOptDouble),
  zcard: spec((key: Data) => av("ZCARD", key), asCount),
  zrange: spec((key: Data, start: number, stop: number) => av("ZRANGE", key, String(start), String(stop)), asBulks),
};

// --- sorted-set algebra (§3.6) ------------------------------------------

function zstoreArgv(
  verb: string,
  dest: Data,
  keys: Data[],
  weights: number[] | null,
  agg: ZAggregate,
): Uint8Array[] {
  nonEmpty(keys, "zset algebra needs at least one source key");
  const parts: Uint8Array[] = [toBytes(verb), toBytes(dest), toBytes(String(keys.length))];
  for (const k of keys) parts.push(toBytes(k));
  if (weights) {
    parts.push(toBytes("WEIGHTS"));
    for (const w of weights) parts.push(toBytes(floatStr(w)));
  }
  if (agg !== "sum") parts.push(toBytes("AGGREGATE"), toBytes(aggregateTag(agg)));
  return parts;
}

export const zalgebraSpecs = {
  zinterstore: spec((dest: Data, ...keys: Data[]) => zstoreArgv("ZINTERSTORE", dest, keys, null, "sum"), asCount),
  zinterstoreWith: spec(
    (dest: Data, keys: Data[], weights: number[] | null, agg: ZAggregate) =>
      zstoreArgv("ZINTERSTORE", dest, keys, weights, agg),
    asCount,
  ),
  zunionstore: spec((dest: Data, ...keys: Data[]) => zstoreArgv("ZUNIONSTORE", dest, keys, null, "sum"), asCount),
  zunionstoreWith: spec(
    (dest: Data, keys: Data[], weights: number[] | null, agg: ZAggregate) =>
      zstoreArgv("ZUNIONSTORE", dest, keys, weights, agg),
    asCount,
  ),
  zintercard: spec((keys: Data[], limit: number | null) => {
    nonEmpty(keys, "zset algebra needs at least one source key");
    const parts: Uint8Array[] = [toBytes("ZINTERCARD"), toBytes(String(keys.length))];
    for (const k of keys) parts.push(toBytes(k));
    if (limit != null && limit >= 0) parts.push(toBytes("LIMIT"), toBytes(String(limit)));
    return parts;
  }, asCount),
};

// --- hash-field TTL (§3.7) ----------------------------------------------

function hashTTLArgv(verb: string, key: Data, arg: string | null, cond: HExpireCond, fields: Data[]): Uint8Array[] {
  nonEmpty(fields, "hash field-TTL verbs need at least one field");
  const parts: Uint8Array[] = [toBytes(verb), toBytes(key)];
  if (arg != null) parts.push(toBytes(arg));
  const kw = condKeyword(cond);
  if (kw) parts.push(toBytes(kw));
  parts.push(toBytes("FIELDS"), toBytes(String(fields.length)));
  for (const f of fields) parts.push(toBytes(f));
  return parts;
}

export const hashttlSpecs = {
  hexpire: spec(
    (key: Data, fields: Data[], ttlMs: number, cond: HExpireCond = "always") =>
      hashTTLArgv("HEXPIRE", key, String(Math.floor((ttlMs < 0 ? 0 : ttlMs) / 1000)), cond, fields),
    asIntList,
  ),
  hpexpire: spec(
    (key: Data, fields: Data[], ttlMs: number, cond: HExpireCond = "always") =>
      hashTTLArgv("HPEXPIRE", key, msArg(ttlMs), cond, fields),
    asIntList,
  ),
  hpersist: spec((key: Data, ...fields: Data[]) => hashTTLArgv("HPERSIST", key, null, "always", fields), asIntList),
  httl: spec((key: Data, ...fields: Data[]) => hashTTLArgv("HTTL", key, null, "always", fields), asIntList),
  hpttl: spec((key: Data, ...fields: Data[]) => hashTTLArgv("HPTTL", key, null, "always", fields), asIntList),
};
