// Runs the wasm module in Node to prove kevy-embedded's core KV works in wasm.
//   wasm-pack build --target nodejs
//   node run.cjs
const { KvCache } = require("./pkg/kevy_wasm_kv.js");

const c = new KvCache();
c.set("hello", "world");
c.set("greeting", "hi");

console.assert(c.get("hello") === "world", "get hello");
console.assert(c.get("missing") === undefined, "missing key -> undefined");
console.assert(c.size() === 2, "dbsize == 2");

console.log("OK - kevy-embedded core KV (set/get/dbsize) runs in wasm in-memory.");
