// Blocking pops: BLPOP / BRPOP / BZPOPMIN (contract §3.14). Remote parks the
// server connection; the pure-FFI embedded backend has no blocking-pop symbol
// (§5.1), so embedded emulates it by polling the non-blocking pop until data
// arrives or the timeout elapses — observably identical, only a bounded poll
// latency differs. timeoutMs null waits forever (wire 0); a 0 is rejected
// (ambiguous, since wire 0 already means forever). Both faces share this.

import { toBytes, unexpectedReply, type Data, type Reply } from "./resp.ts";
import { InvalidInputError } from "./errors.ts";
import { av, replyError, scoreOf } from "./reply.ts";
import { sleepSync } from "./ffi.ts";
import type { Backend } from "./backend.ts";

/** A (key, value) blocking-pop hit. */
export interface KV {
  key: Uint8Array;
  value: Uint8Array;
}

/** A (key, member, score) BZPOPMIN hit (contract §4.9). */
export interface ZPopHit {
  key: Uint8Array;
  member: Uint8Array;
  score: number;
}

function check(keys: Data[], timeoutMs: number | null): void {
  if (keys.length === 0) throw new InvalidInputError("blocking pop needs at least one key");
  if (timeoutMs === 0) {
    throw new InvalidInputError("timeout 0 is ambiguous (wire 0 = wait forever); use null to wait forever");
  }
}

function blockingArgv(verb: string, keys: Data[], timeoutMs: number | null): Uint8Array[] {
  const secs = timeoutMs == null ? "0" : String(timeoutMs / 1000);
  return av(verb, ...keys, secs);
}

function popKV(r: Reply): KV | null {
  if (r.kind === "error") replyError(r);
  if (r.kind === "array" && r.items.length === 2) {
    const [k, v] = r.items;
    if (k!.kind === "bulk" && v!.kind === "bulk") return { key: k!.bytes, value: v!.bytes };
  }
  if (r.kind === "nil" || r.kind === "null") return null;
  throw unexpectedReply(r);
}

function popZ(r: Reply): ZPopHit | null {
  if (r.kind === "error") replyError(r);
  if (r.kind === "array" && r.items.length === 3) {
    const [k, m, s] = r.items;
    if (k!.kind === "bulk" && m!.kind === "bulk") return { key: k!.bytes, member: m!.bytes, score: scoreOf(s!) };
  }
  if (r.kind === "nil" || r.kind === "null") return null;
  throw unexpectedReply(r);
}

// firstBulk extracts the popped value from a non-blocking LPOP/RPOP count-1.
function firstBulk(r: Reply): Uint8Array | null {
  if (r.kind === "array" && r.items.length > 0 && r.items[0]!.kind === "bulk") return r.items[0]!.bytes;
  if (r.kind === "bulk") return r.bytes;
  return null;
}

function firstZ(r: Reply): [Uint8Array, number] | null {
  if (r.kind === "array" && r.items.length >= 2 && r.items[0]!.kind === "bulk") {
    return [r.items[0]!.bytes, scoreOf(r.items[1]!)];
  }
  return null;
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// --- async (both backends) ----------------------------------------------

export async function blpopAsync(be: Backend, verb: "LPOP" | "RPOP", keys: Data[], timeoutMs: number | null): Promise<KV | null> {
  check(keys, timeoutMs);
  if (!be.isEmbedded) return popKV(await be.exec(blockingArgv(verb === "LPOP" ? "BLPOP" : "BRPOP", keys, timeoutMs)));
  const deadline = timeoutMs == null ? null : Date.now() + timeoutMs;
  for (;;) {
    for (const k of keys) {
      const hit = firstBulk(await be.exec(av(verb, k, "1")));
      if (hit) return { key: toBytes(k), value: hit };
    }
    if (deadline != null && Date.now() >= deadline) return null;
    await sleep(5);
  }
}

export async function bzpopminAsync(be: Backend, keys: Data[], timeoutMs: number | null): Promise<ZPopHit | null> {
  check(keys, timeoutMs);
  if (!be.isEmbedded) return popZ(await be.exec(blockingArgv("BZPOPMIN", keys, timeoutMs)));
  const deadline = timeoutMs == null ? null : Date.now() + timeoutMs;
  for (;;) {
    for (const k of keys) {
      const hit = firstZ(await be.exec(av("ZPOPMIN", k, "1")));
      if (hit) return { key: toBytes(k), member: hit[0], score: hit[1] };
    }
    if (deadline != null && Date.now() >= deadline) return null;
    await sleep(5);
  }
}

// --- sync (embedded-only; remote execSync throws Unsupported) ------------

export function blpopSync(be: Backend, verb: "LPOP" | "RPOP", keys: Data[], timeoutMs: number | null): KV | null {
  check(keys, timeoutMs);
  const deadline = timeoutMs == null ? null : Date.now() + timeoutMs;
  for (;;) {
    for (const k of keys) {
      const hit = firstBulk(be.execSync(av(verb, k, "1")));
      if (hit) return { key: toBytes(k), value: hit };
    }
    if (deadline != null && Date.now() >= deadline) return null;
    sleepSync(5);
  }
}

export function bzpopminSync(be: Backend, keys: Data[], timeoutMs: number | null): ZPopHit | null {
  check(keys, timeoutMs);
  const deadline = timeoutMs == null ? null : Date.now() + timeoutMs;
  for (;;) {
    for (const k of keys) {
      const hit = firstZ(be.execSync(av("ZPOPMIN", k, "1")));
      if (hit) return { key: toBytes(k), member: hit[0], score: hit[1] };
    }
    if (deadline != null && Date.now() >= deadline) return null;
    sleepSync(5);
  }
}
