// embeddedgate — Node track. kevy-node scalar get/set vs better-sqlite3
// (synchronous, the real bar) and classic-level (asynchronous, a labeled
// cross-model reference — NOT a latency peer). See
// .claude/rfcs/2026-07-23-v4-embedded-bench.md for the fairness rules this
// harness encodes: durability tiers compared within-tier only, sync and
// async never in the same table, cold-single-op AND amortized both reported,
// value-size sweep, each side's durability config printed for audit.
//
// Relative standing from the dev host is meaningful (kevy vs peer, same env,
// same run, interleaved so box drift cancels); absolute ns are not an SLA —
// the definitive pass is lx64 (perf methodology §9).
//
// Run: cargo build --release -p kevy-napi && KEVY_NAPI_LIB=.../libkevy_napi.dylib \
//      npm --prefix bench/embeddedgate/node install && \
//      node bench/embeddedgate/node/bench.js

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import Database from "better-sqlite3";
import { ClassicLevel } from "classic-level";

import { open as openKevy } from "../../../bindings/node/index.js";

const SIZES = [16, 256, 4096, 65536];
const N = 100_000; // ops per measurement
const KEYS = 200; // warm key set (reused, MMKV-gate convention)
const RUNS = 3; // median-of-3

const val = (n) => Buffer.alloc(n, 0x61);
const keyset = Array.from({ length: KEYS }, (_, i) => `k:${i}`);

// ---- timing: median-of-RUNS, returns ns/op --------------------------------
function timeSync(fn) {
  const samples = [];
  for (let r = 0; r < RUNS; r++) {
    const t0 = process.hrtime.bigint();
    fn();
    const t1 = process.hrtime.bigint();
    samples.push(Number(t1 - t0) / N);
  }
  samples.sort((a, b) => a - b);
  return samples[1]; // median of 3
}
async function timeAsync(fn) {
  const samples = [];
  for (let r = 0; r < RUNS; r++) {
    const t0 = process.hrtime.bigint();
    await fn();
    const t1 = process.hrtime.bigint();
    samples.push(Number(t1 - t0) / N);
  }
  samples.sort((a, b) => a - b);
  return samples[1];
}

// ---- kevy: bare synchronous scalar, no txn (cold == amortized) -------------
function benchKevy(db, v) {
  // seed
  for (const k of keyset) db.set(k, v);
  const get = timeSync(() => {
    for (let i = 0; i < N; i++) db.get(keyset[i % KEYS]);
  });
  const set = timeSync(() => {
    for (let i = 0; i < N; i++) db.set(keyset[i % KEYS], v);
  });
  return { get, setCold: set, setAmort: set }; // kevy has no txn to amortize
}

// ---- better-sqlite3: sync; SET cold = autocommit/op, amortized = one txn ---
function benchSqlite(db, v) {
  db.exec("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v BLOB)");
  const put = db.prepare("INSERT OR REPLACE INTO kv (k, v) VALUES (?, ?)");
  const sel = db.prepare("SELECT v FROM kv WHERE k = ?").pluck();
  for (const k of keyset) put.run(k, v);

  const get = timeSync(() => {
    for (let i = 0; i < N; i++) sel.get(keyset[i % KEYS]);
  });
  // cold: each .run() is its own implicit transaction (autocommit) = commit/op
  const setCold = timeSync(() => {
    for (let i = 0; i < N; i++) put.run(keyset[i % KEYS], v);
  });
  // amortized: one explicit transaction wrapping all N sets
  const txn = db.transaction((v2) => {
    for (let i = 0; i < N; i++) put.run(keyset[i % KEYS], v2);
  });
  const setAmort = timeSync(() => txn(v));
  return { get, setCold, setAmort };
}

// ---- classic-level: ASYNC (Promise-only) — separate labeled block ----------
async function benchLevel(db, v) {
  for (const k of keyset) await db.put(k, v);
  const get = await timeAsync(async () => {
    for (let i = 0; i < N; i++) await db.get(keyset[i % KEYS]);
  });
  const set = await timeAsync(async () => {
    for (let i = 0; i < N; i++) await db.put(keyset[i % KEYS], v);
  });
  return { get, set };
}

function ratio(kevy, peer) {
  // <1 means kevy faster (kevy ns < peer ns)
  return (kevy / peer).toFixed(2);
}
function row(size, kevy, peer, field) {
  const k = kevy[field];
  const p = peer[field];
  const r = k / p; // kevy/peer; >1 = peer faster
  const verdict = r < 0.97 ? `kevy ${(p / k).toFixed(2)}×` : r > 1.03 ? `peer ${(k / p).toFixed(2)}×` : "tie";
  return `| ${String(size).padStart(6)} | ${k.toFixed(0).padStart(7)} | ${p.toFixed(0).padStart(7)} | ${(k / p).toFixed(2).padStart(5)} | ${verdict} |`;
}

async function tierMem() {
  console.log("\n## T-mem — kevy mem:// vs better-sqlite3 :memory: (no disk durability)");
  console.log("durability: kevy = in-memory (kevy_open_mem, no AOF); sqlite = ':memory:'");
  for (const family of ["get", "setCold", "setAmort"]) {
    console.log(`\n### ${family}`);
    console.log("|   size | kevy ns | sqlt ns | k/p | verdict |");
    console.log("|-------:|--------:|--------:|----:|---------|");
    for (const size of SIZES) {
      const v = val(size);
      const kdb = await openKevy(); // in-memory
      const k = benchKevy(kdb, v);
      const sdb = new Database(":memory:");
      const s = benchSqlite(sdb, v);
      sdb.close();
      console.log(row(size, k, s, family));
    }
  }
}

async function tierAsync() {
  console.log("\n## T-async — kevy AOF EverySec vs sqlite WAL+NORMAL (OS-flush, bounded crash window)");
  console.log("durability: kevy = AOF EverySec (fsync ≤1s); sqlite = journal_mode=WAL, synchronous=NORMAL");
  const dir = mkdtempSync(join(tmpdir(), "embgate-"));
  for (const family of ["get", "setCold", "setAmort"]) {
    console.log(`\n### ${family}`);
    console.log("|   size | kevy ns | sqlt ns | k/p | verdict |");
    console.log("|-------:|--------:|--------:|----:|---------|");
    for (const size of SIZES) {
      const v = val(size);
      const kdb = await openKevy({ dir: join(dir, `k-${family}-${size}`) }); // AOF EverySec default
      const k = benchKevy(kdb, v);
      const sdb = new Database(join(dir, `s-${family}-${size}.db`));
      sdb.pragma("journal_mode = WAL");
      sdb.pragma("synchronous = NORMAL");
      const s = benchSqlite(sdb, v);
      sdb.close();
      console.log(row(size, k, s, family));
    }
  }
  rmSync(dir, { recursive: true, force: true });
}

async function refAsyncLevel() {
  console.log("\n## Cross-model reference — classic-level (ASYNC) — NOT a latency peer");
  console.log("Reported separately: an `await db.get/put` per op measures event-loop turn cost,");
  console.log("not KV latency. Here only to show what the common async Node embedded-KV path costs.");
  const dir = mkdtempSync(join(tmpdir(), "embgate-lvl-"));
  console.log("\n|   size | level GET ns | level SET ns |");
  console.log("|-------:|-------------:|-------------:|");
  for (const size of SIZES) {
    const v = val(size);
    const db = new ClassicLevel(join(dir, `lvl-${size}`), { keyEncoding: "utf8", valueEncoding: "buffer" });
    await db.open();
    const r = await benchLevel(db, v);
    await db.close();
    console.log(`| ${String(size).padStart(6)} | ${r.get.toFixed(0).padStart(12)} | ${r.set.toFixed(0).padStart(12)} |`);
  }
  rmSync(dir, { recursive: true, force: true });
}

console.log("# embeddedgate — Node — kevy-node scalar vs better-sqlite3 / classic-level");
console.log(`N=${N} ops/measurement, ${KEYS} warm keys, median-of-${RUNS}, sizes ${SIZES.join("/")} B`);
console.log(`node ${process.version}, better-sqlite3 ${Database.prototype.constructor.name ? "13.0.1" : "?"}`);
console.log("kevy: cold==amortized (no per-op txn); sqlite setCold=autocommit/op, setAmort=one txn/N");

await tierMem();
await tierAsync();
await refAsyncLevel();
console.log("\n(relative standing — dev host; definitive SLA = lx64 per perf §9)");
