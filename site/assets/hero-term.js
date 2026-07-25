// The landing page's terminal is not a mock-up. It boots the same kevy engine
// the server runs — compiled to WebAssembly — and every reply it prints came
// out of that engine a millisecond ago.
//
// That is the one thing no other database's landing page can do, and it is
// also the most honest possible pitch: the visitor's first interaction with
// kevy is USING kevy, not reading about it. No canned output exists in this
// file; if the engine failed to boot, the terminal says so instead of faking.
//
// Scope honesty: the wasm build carries the KV core — strings, TTLs, pub/sub.
// The vector/index examples elsewhere on the page are server commands, and a
// CI gate executes every one of them against a real server. The terminal only
// offers what it can truly run.

const $ = (s, r) => (r || document).querySelector(s);

const TERM = $("#hero-term");
if (TERM) boot().catch(fail);

async function boot() {
  // Resolved against the page URL: a bare "demo/pkg" from the root page is a
  // bare module specifier and import() rejects it outright.
  const base = new URL(TERM.dataset.pkg + "/", location.href);
  const { open } = await import(new URL("kevy.js", base));
  // In-memory: the landing page should not write to the visitor's disk
  // uninvited. The playground is where persistence gets demonstrated.
  const db = await open({ wasm: new URL("kevy.wasm", base).href, persist: false, tickMs: 100 });

  const out = $(".ht-out", TERM);
  const input = $(".ht-in", TERM);
  const status = $(".ht-status", TERM);
  status.textContent = "live";
  status.classList.add("on");

  const enc = new TextEncoder();
  const dec = new TextDecoder();

  function print(line, cls) {
    const el = document.createElement("div");
    el.className = `ht-line ${cls || ""}`;
    el.textContent = line;
    out.appendChild(el);
    // keep the window shallow — this is a hero, not a log viewer
    while (out.children.length > 14) out.firstChild.remove();
    out.scrollTop = out.scrollHeight;
  }

  // A deliberately small command surface: exactly what the wasm ABI offers.
  // Anything else gets a pointer to the playground rather than a fake error.
  function run(raw) {
    const line = raw.trim();
    if (!line) return;
    print(`> ${line}`, "cmd");
    const parts = split(line);
    const verb = (parts[0] || "").toUpperCase();
    try {
      switch (verb) {
        case "SET": {
          if (parts.length < 3) return print("(error) wrong number of arguments", "err");
          let ttl;
          const exAt = parts.findIndex((p, i) => i >= 3 && p.toUpperCase() === "EX");
          if (exAt > 0 && parts[exAt + 1]) ttl = parseFloat(parts[exAt + 1]) * 1000;
          db.set(parts[1], parts[2], ttl ? { ttlMs: ttl } : undefined);
          return print("OK", "ok");
        }
        case "GET": {
          const v = db.getText(parts[1]);
          return print(v === undefined ? "(nil)" : `"${v}"`, v === undefined ? "dim" : "ok");
        }
        case "TTL": {
          const ms = db.pttl(parts[1]);
          return print(`(integer) ${ms < 0 ? ms : Math.round(ms / 1000)}`, "ok");
        }
        case "INCR": {
          return print(`(integer) ${db.incrby(parts[1], 1)}`, "ok");
        }
        case "DEL": {
          return print(`(integer) ${db.del(parts[1]) ? 1 : 0}`, "ok");
        }
        case "KEYS": {
          const ks = db.keys(parts[1] || "*", 8);
          if (!ks.length) return print("(empty array)", "dim");
          ks.forEach((k, i) => print(`${i + 1}) "${k}"`, "ok"));
          return;
        }
        case "DBSIZE": {
          return print(`(integer) ${db.dbsize()}`, "ok");
        }
        case "SUBSCRIBE": {
          const ch = parts[1] || "news";
          db.subscribe(ch, (payload) =>
            print(`(message) ${ch}: ${dec.decode(payload)}`, "msg"),
          );
          return print(`subscribed to "${ch}"`, "ok");
        }
        case "PUBLISH": {
          const n = db.publish(parts[1], parts.slice(2).join(" "));
          return print(`(integer) ${n}`, "ok");
        }
        default:
          return print(
            `"${verb}" is not in this demo — the playground has the full keyspace view`,
            "dim",
          );
      }
    } catch (e) {
      print(`(error) ${e.message || e}`, "err");
    }
  }

  // Quoted-string aware split, so SET user '{"name":"ada"}' works.
  function split(line) {
    const out2 = [];
    let cur = "";
    let q = null;
    for (const ch of line) {
      if (q) {
        if (ch === q) q = null;
        else cur += ch;
      } else if (ch === "'" || ch === '"') q = ch;
      else if (ch === " ") {
        if (cur) out2.push(cur), (cur = "");
      } else cur += ch;
    }
    if (cur) out2.push(cur);
    return out2;
  }

  // The chips: one-click commands, in a sequence that tells the story —
  // write with a TTL, read it, watch the clock, count, listen, publish.
  for (const chip of TERM.querySelectorAll(".ht-chip")) {
    chip.addEventListener("click", () => {
      run(chip.dataset.cmd);
      input.focus();
    });
  }

  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      run(input.value);
      input.value = "";
    }
  });

  // One unprompted beat so the terminal is visibly alive before anyone
  // touches it: a session key with a real TTL, then its clock, ticking.
  run('SET session:7f3a \'{"user":"ada"}\' EX 90');
  run("TTL session:7f3a");
}

function fail(e) {
  const status = $(".ht-status", TERM);
  const out = $(".ht-out", TERM);
  if (status) status.textContent = "engine failed to load";
  if (out) {
    const el = document.createElement("div");
    el.className = "ht-line err";
    el.textContent = String(e);
    out.appendChild(el);
  }
}
