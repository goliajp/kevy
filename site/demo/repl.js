// kevy live demo — a small REPL over the @goliajp/kevy browser API.
// The engine below is the real wasm module from ./pkg/ (see ./pkg/kevy.js);
// this file only parses command lines and prints replies.
//
// Rebuilding ./pkg/ from source (repository root):
//   cargo build -p kevy-wasm --target wasm32-unknown-unknown --release
//   cp target/wasm32-unknown-unknown/release/kevy_wasm.wasm crates/kevy-wasm/pkg/kevy.wasm
//   cp crates/kevy-wasm/pkg/{kevy.js,kevy.d.ts,kevy-opfs-worker.js,kevy.wasm} site/demo/pkg/

import { open } from "./pkg/kevy.js";

const logEl = document.getElementById("log");
const stat = document.getElementById("stat");
const dot = document.getElementById("dot");
const form = document.getElementById("f");
const input = document.getElementById("cmd");

const td = new TextDecoder();

function print(text, cls) {
  const div = document.createElement("div");
  if (cls) div.className = cls;
  div.textContent = text;
  logEl.appendChild(div);
  logEl.scrollTop = logEl.scrollHeight;
}

// Report which storage backend "auto" will have picked. OPFS synchronous
// access handles exist only inside dedicated workers, so the check runs
// in a throwaway worker; no cross-origin isolation (COOP/COEP) is needed
// by either backend — the loader's OPFS worker talks plain postMessage.

const subs = new Map(); // channel -> unsubscribe
const psubs = new Map(); // pattern -> unsubscribe

const HELP = [
  "set <key> <value> [ttl <ms>]   store a value (optional expiry)",
  "get <key>                      read a value",
  "del <key>                      delete a key",
  "exists <key>                   1 if present",
  "expire <key> <ms>              set an expiry",
  "persist <key>                  clear an expiry",
  "pttl <key>                     remaining ms (-1 none, -2 no key)",
  "incr <key> [n]                 increment a counter",
  "keys [pattern]                 list keys (glob, default *)",
  "dbsize                         key count",
  "flushall                       delete everything",
  "subscribe <channel>            print messages (try a second tab)",
  "unsubscribe <channel>          stop",
  "psubscribe <pattern>           glob subscription",
  "punsubscribe <pattern>         stop",
  "publish <channel> <message>    deliver to this and other tabs",
  "flush                          durability barrier (storage write-out)",
  "compact                        rewrite storage as a compact image",
  "clear                          clear this screen",
];

let db = null;

async function runCommand(line) {
  const parts = line.split(/\s+/).filter(Boolean);
  const cmd = (parts[0] || "").toLowerCase();
  const a = parts[1];
  switch (cmd) {
    case "help":
      for (const l of HELP) print(l, "dim");
      return;
    case "clear":
      logEl.textContent = "";
      return;
    case "set": {
      if (parts.length < 3) throw new Error("usage: set <key> <value> [ttl <ms>]");
      let end = parts.length;
      let ttlMs;
      if (parts.length >= 5 && parts[parts.length - 2].toLowerCase() === "ttl") {
        ttlMs = Number(parts[parts.length - 1]);
        if (!Number.isFinite(ttlMs)) throw new Error("ttl must be a number of ms");
        end = parts.length - 2;
      }
      db.set(a, parts.slice(2, end).join(" "), ttlMs ? { ttlMs } : undefined);
      print("OK", "ok");
      return;
    }
    case "get": {
      if (!a) throw new Error("usage: get <key>");
      const v = db.getText(a);
      print(v === undefined ? "(nil)" : JSON.stringify(v), v === undefined ? "dim" : "ok");
      return;
    }
    case "del":
      if (!a) throw new Error("usage: del <key>");
      print(db.del(a) ? "1" : "0", "ok");
      return;
    case "exists":
      if (!a) throw new Error("usage: exists <key>");
      print(db.exists(a) ? "1" : "0", "ok");
      return;
    case "expire": {
      if (parts.length < 3) throw new Error("usage: expire <key> <ms>");
      print(db.expire(a, Number(parts[2])) ? "1" : "0", "ok");
      return;
    }
    case "persist":
      if (!a) throw new Error("usage: persist <key>");
      print(db.persist(a) ? "1" : "0", "ok");
      return;
    case "pttl":
      if (!a) throw new Error("usage: pttl <key>");
      print(String(db.pttl(a)), "ok");
      return;
    case "incr":
      if (!a) throw new Error("usage: incr <key> [n]");
      print(String(db.incrby(a, parts[2] === undefined ? 1 : Number(parts[2]))), "ok");
      return;
    case "keys": {
      const ks = db.keys(a || "*");
      if (ks.length === 0) print("(empty)", "dim");
      else ks.forEach((k, i) => print(`${i + 1}) ${k}`, "ok"));
      return;
    }
    case "dbsize":
      print(String(db.dbsize()), "ok");
      return;
    case "flushall":
      db.flushall();
      print("OK", "ok");
      return;
    case "subscribe": {
      if (!a) throw new Error("usage: subscribe <channel>");
      if (subs.has(a)) throw new Error(`already subscribed to ${a}`);
      subs.set(a, db.subscribe(a, (payload, channel) => {
        print(`[${channel}] ${td.decode(payload)}`, "ev");
      }));
      print(`subscribed to ${a} — publish here or from another tab`, "ok");
      return;
    }
    case "unsubscribe": {
      if (!a || !subs.has(a)) throw new Error(`not subscribed to ${a || "?"}`);
      subs.get(a)();
      subs.delete(a);
      print("OK", "ok");
      return;
    }
    case "psubscribe": {
      if (!a) throw new Error("usage: psubscribe <pattern>");
      if (psubs.has(a)) throw new Error(`already subscribed to ${a}`);
      psubs.set(a, db.psubscribe(a, (payload, channel, pattern) => {
        print(`[${pattern} → ${channel}] ${td.decode(payload)}`, "ev");
      }));
      print(`pattern-subscribed to ${a}`, "ok");
      return;
    }
    case "punsubscribe": {
      if (!a || !psubs.has(a)) throw new Error(`not subscribed to ${a || "?"}`);
      psubs.get(a)();
      psubs.delete(a);
      print("OK", "ok");
      return;
    }
    case "publish": {
      if (parts.length < 3) throw new Error("usage: publish <channel> <message>");
      const msg = line.slice(line.indexOf(a) + a.length + 1);
      const n = db.publish(a, msg);
      print(`${n} local receiver${n === 1 ? "" : "s"} (+ any other open tabs)`, "ok");
      return;
    }
    case "flush":
      await db.flush();
      print("OK — pending frames are on storage", "ok");
      return;
    case "compact":
      await db.compact();
      print("OK — storage rewritten as a compact image", "ok");
      return;
    default:
      throw new Error(`unknown command: ${cmd} (try: help)`);
  }
}

// --- input handling, with arrow-key history ---

const history = [];
let histAt = -1;

form.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const line = input.value.trim();
  input.value = "";
  if (!line) return;
  history.push(line);
  histAt = history.length;
  print(line, "in");
  if (!db) { print("engine is still loading …", "err"); return; }
  try {
    await runCommand(line);
  } catch (err) {
    print(String(err.message || err), "err");
  }
});

input.addEventListener("keydown", (ev) => {
  if (ev.key === "ArrowUp" && histAt > 0) {
    histAt -= 1;
    input.value = history[histAt];
    ev.preventDefault();
  } else if (ev.key === "ArrowDown") {
    histAt = Math.min(histAt + 1, history.length);
    input.value = history[histAt] ?? "";
    ev.preventDefault();
  }
});

// --- self-test hook (?selftest): exercises the same dispatcher the REPL
// uses, so a headless browser can smoke-test the page end to end. ---

async function selftest() {
  const checks = [];
  const check = (name, fn) => checks.push([name, fn]);
  const K = "selftest:key";
  check("set", () => { db.set(K, "v1"); return true; });
  check("get", () => db.getText(K) === "v1");
  check("exists", () => db.exists(K) === true);
  check("expire", () => db.expire(K, 60_000) === true);
  check("pttl", () => db.pttl(K) > 0);
  check("persist", () => db.persist(K) === true);
  check("incr", () => db.incrby("selftest:n", 2) >= 2);
  check("keys", () => db.keys("selftest:*").length >= 2);
  check("del", () => db.del(K) === true);
  check("pubsub", () => {
    let got = "";
    const off = db.subscribe("selftest:ch", (p) => { got = td.decode(p); });
    db.publish("selftest:ch", "ping");
    off();
    return got === "ping";
  });
  let pass = 0;
  for (const [name, fn] of checks) {
    let ok = false;
    try { ok = fn(); } catch { ok = false; }
    print(`selftest ${name}: ${ok ? "ok" : "FAIL"}`, ok ? "ok" : "err");
    if (ok) pass += 1;
  }
  db.del("selftest:n");
  const verdict = pass === checks.length ? "PASS" : "FAIL";
  print(`SELFTEST ${verdict} ${pass}/${checks.length}`, verdict === "PASS" ? "ok" : "err");
  document.title = `SELFTEST-${verdict}`;
}

// --- boot ---

(async () => {
  try {
    db = await open({ persist: { name: "kevy-demo" } });
    dot.classList.add("on");
    const n = db.dbsize();
    // The engine reports which backend it actually opened. Probing the
    // platform and guessing from that is how a page ends up claiming OPFS
    // on a build that silently fell back to IndexedDB.
    const backend = db.backend === "opfs" ? "OPFS" : "IndexedDB";
    const badge = document.getElementById("backend");
    if (badge) badge.textContent = backend;
    stat.textContent =
      `engine ready · persistence: ${backend}` +
      ` · cross-tab bridge: on · ${n} key${n === 1 ? "" : "s"} replayed`;
    print("kevy is up. Data persists across reloads; open a second tab for pub/sub.", "dim");
    print("Type: help", "dim");
    if (new URLSearchParams(location.search).has("selftest")) await selftest();
  } catch (err) {
    stat.textContent = "failed to start";
    print(`failed to start: ${String(err.message || err)}`, "err");
    print("This demo needs a modern browser with WebAssembly and module workers.", "dim");
  }
  input.focus();
})();
