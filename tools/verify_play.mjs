// Drive the playground in a real Chrome over CDP and assert on what the PAGE
// ends up showing — not on what the source says it should show.
//
// The thing being checked is not "does the JS run". It is: does a button press
// put a real key in a real engine, does the TTL bar come from the engine's clock
// rather than a setInterval on the page, and are the bytes in the log panel
// actually on disk. Each of those is a claim the site makes to a stranger.

import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Chrome, wherever it lives. CI has `google-chrome`; a mac has the app bundle.
const CHROME =
  process.env.CHROME ??
  ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
   "/usr/bin/google-chrome",
   "/usr/bin/chromium"].find((p) => existsSync(p));
if (!CHROME) {
  console.log("  SKIP  no Chrome found (set CHROME=/path/to/chrome)");
  process.exit(0);
}
const PORT = 9222;
const URL = process.argv[2] || "http://127.0.0.1:8901/play/";

const chrome = spawn(CHROME, [
  `--remote-debugging-port=${PORT}`,
  "--headless=new",
  "--no-first-run",
  `--user-data-dir=${mkdtempSync(join(tmpdir(), "kevy-cdp-"))}`,
  "about:blank",
], { stdio: "ignore" });

const fail = [];
const ok = [];
const check = (name, cond, detail = "") =>
  (cond ? ok : fail).push(`${name}${detail ? " — " + detail : ""}`);

try {
  await sleep(1600);
  const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
  const page = list.find((t) => t.type === "page");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));

  let id = 0;
  const pending = new Map();
  const errors = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) pending.get(m.id)(m);
    if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error") {
      errors.push(m.params.args.map((a) => a.value ?? a.description).join(" "));
    }
    if (m.method === "Runtime.exceptionThrown") {
      errors.push(m.params.exceptionDetails.exception?.description ?? "exception");
    }
  };
  const send = (method, params = {}) =>
    new Promise((res) => {
      const i = ++id;
      pending.set(i, res);
      ws.send(JSON.stringify({ id: i, method, params }));
    });
  const evalJs = async (expr) => {
    const r = await send("Runtime.evaluate", {
      expression: expr,
      awaitPromise: true,
      returnByValue: true,
    });
    if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.text);
    return r.result?.result?.value;
  };

  await send("Runtime.enable");
  await send("Page.enable");
  await send("Page.navigate", { url: URL });
  await sleep(3500); // wasm compile + OPFS worker

  // ── the engine actually booted ──────────────────────────────────────────
  const booted = await evalJs(`!document.getElementById("app").hidden`);
  check("engine boots", booted);
  if (!booted) throw new Error("app never became visible");

  const backend = await evalJs(`document.getElementById("stat-backend").textContent`);
  check("durable backend in use", backend === "opfs" || backend === "idb", `backend=${backend}`);

  // ── a scenario button writes REAL keys ──────────────────────────────────
  await evalJs(`document.querySelector('[data-scenario="session"]').click()`);
  await sleep(400);
  const keys = await evalJs(`Number(document.getElementById("stat-keys").textContent)`);
  check("scenario writes keys", keys === 3, `dbsize=${keys}`);

  const rows = await evalJs(
    `[...document.querySelectorAll("#ks-body tr")].map(r => r.querySelector(".k")?.textContent)`,
  );
  check("keys land in the keyspace panel", rows.filter(Boolean).length === 3, rows.join(" "));

  // ── the TTL bar is the ENGINE's clock, not a page animation ─────────────
  const t1 = await evalJs(
    `parseFloat(document.querySelector("#ks-body .ttl-n").textContent)`,
  );
  await sleep(1500);
  const t2 = await evalJs(
    `parseFloat(document.querySelector("#ks-body .ttl-n").textContent)`,
  );
  check(
    "TTL counts down from the engine",
    t2 < t1 && t1 - t2 > 1.0 && t1 - t2 < 2.2,
    `${t1}s -> ${t2}s`,
  );

  // ── the log panel holds REAL bytes read back out of OPFS ────────────────
  const bytes = await evalJs(`document.getElementById("stat-bytes").textContent`);
  const n = parseInt(bytes.replace(/[^\d]/g, ""), 10);
  check("log has bytes on disk", n > 0, bytes);

  const aof = await evalJs(`document.getElementById("aof").textContent`);
  check(
    "log is RESP, and holds the key we just wrote",
    aof.includes("session:") && aof.includes("SET"),
    aof.slice(0, 60).replace(/\n/g, "\\n"),
  );

  // and it really is on disk, not in a variable — read it back independently
  const onDisk = await evalJs(`(async () => {
    const root = await navigator.storage.getDirectory();
    const dir = await root.getDirectoryHandle("kevy-wasm");
    const fh = await dir.getFileHandle("playground.aof");
    return (await fh.getFile()).size;
  })()`);
  check("OPFS file exists independently of the page", onDisk > 0, `${onDisk} B`);

  // ── the TTL actually EXPIRES the key (the engine evicts, unprompted) ────
  await evalJs(`kevyTestExpire = document.getElementById("stat-keys").textContent`);
  await sleep(8000); // the 8-second session
  const after = await evalJs(`Number(document.getElementById("stat-keys").textContent)`);
  check("engine evicts the expired key on its own", after === 2, `dbsize=${after} (was 3)`);

  // ── pub/sub round-trips through the engine ─────────────────────────────
  await evalJs(`
    document.getElementById("pm").value = "hello";
    document.getElementById("p").dispatchEvent(new Event("submit", {cancelable:true}));
  `);
  await sleep(300);
  const items = await evalJs(
    `[...document.querySelectorAll("#feed li")].map(li => li.className + ":" + li.querySelector(".msg")?.textContent)`,
  );
  check("publish shows in the feed EXACTLY once", items.length === 1, JSON.stringify(items));
  check("and is labelled as this tab's own", items[0]?.startsWith("mine:"), items[0]);

  // ── the reload claim: keys survive ─────────────────────────────────────
  await send("Page.navigate", { url: URL });
  await sleep(3500);
  const survived = await evalJs(`Number(document.getElementById("stat-keys").textContent)`);
  check("keys survive a reload (the log replays)", survived >= 2, `dbsize=${survived}`);

  check("no console errors", errors.length === 0, errors.slice(0, 2).join(" | "));
} catch (e) {
  fail.push(`harness: ${e.message}`);
} finally {
  chrome.kill();
}

for (const o of ok) console.log(`  PASS  ${o}`);
for (const f of fail) console.log(`  FAIL  ${f}`);
console.log(`\n  ${ok.length} passed, ${fail.length} failed`);
process.exit(fail.length ? 1 : 0);
