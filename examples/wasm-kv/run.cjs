// Runs the wasm module in Node to prove kevy-embedded works in wasm —
// including TTL (which used to trap on wasm32-unknown-unknown).
//   wasm-pack build --target nodejs
//   node run.cjs
const { KvCache } = require("./pkg/kevy_wasm_kv.js");

const c = new KvCache();

// --- core KV (set/get/dbsize) ---
c.set("hello", "world");
c.set("greeting", "hi");
console.assert(c.get("hello") === "world", "get hello");
console.assert(c.get("missing") === undefined, "missing key -> undefined");
console.assert(c.size() === 2, "dbsize == 2");

// --- del (used to trap: del reaps, reaping read Instant::now()) ---
console.assert(c.del("greeting") === 1, "del greeting -> 1");
console.assert(c.del("greeting") === 0, "del greeting again -> 0");
console.assert(c.size() === 1, "dbsize == 1 after del");

// --- TTL (used to trap on every set_with_ttl/pttl/tick) ---
// Feed the host clock, then write a key with a 50 ms TTL.
let t0 = Date.now();
c.set_clock(t0);
c.set_with_ttl("session", "abc", 50);
console.assert(c.get("session") === "abc", "ttl key readable before expiry");
const ttl = c.pttl("session");
console.assert(ttl > 0 && ttl <= 50, `pttl in (0,50], got ${ttl}`);

// Advance the host clock past the deadline; an expired key reads as absent.
// (Under maxmemory=0, GET takes a read lock, so it reports the key gone but
// leaves physical removal to the reaper — Redis's lazy+active expiry split.)
c.set_clock(t0 + 100);
console.assert(c.get("session") === undefined, "ttl key reads absent after deadline");

// The active reaper sweep collects expired keys (the never-re-touched one
// plus the read-but-not-yet-removed one). Assert behaviour, not exact count.
c.set_clock(t0 + 200);
c.set_with_ttl("temp", "x", 10);
c.set_clock(t0 + 400);
const swept = c.tick();
console.assert(swept >= 1, `reaper swept expired keys, got ${swept}`);
console.assert(c.get("temp") === undefined, "reaped key absent");

// A no-TTL key set before the clock moved must still be live; only `hello`
// remains physically after the sweep.
console.assert(c.get("hello") === "world", "non-expiring key survives");
console.assert(c.size() === 1, `only the non-expiring key remains, size=${c.size()}`);

console.log(
  "OK - kevy-embedded runs in wasm: core KV + del + TTL (set_with_ttl/pttl/lazy+active expiry) all work with a host-fed clock."
);
