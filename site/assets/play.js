// The playground. Not a REPL.
//
// A terminal emulator on a web page proves nothing a screenshot could not fake.
// What is worth showing is the part you cannot see in a terminal: the keyspace
// changing as you press a button, a TTL counting itself to death, the actual
// append-only log accumulating bytes in your browser's own filesystem, and a
// message crossing between two tabs. So: four live panels, buttons instead of a
// prompt, and every number on screen read back out of the engine rather than
// remembered by the page.
//
// The engine is the same kevy-store that runs on the server, compiled to wasm.
// No server is involved — close the network tab and it keeps working.

const LANG = document.documentElement.lang.startsWith("zh")
  ? "zh"
  : document.documentElement.lang.startsWith("ja")
    ? "ja"
    : "en";

const T = {
  en: {
    keyspace: "Keyspace",
    ttl: "Expiry",
    storage: "Durability",
    pubsub: "Pub/Sub across tabs",
    key: "Key",
    value: "Value",
    expires: "Expires",
    empty: "Nothing stored yet — run a scenario, or write a key below.",
    noTtl: "no expiry",
    keyPh: "key",
    valPh: "value",
    ttlPh: "ttl (s)",
    write: "Write",
    del: "Delete",
    flush: "Flush all",
    scenario: "Load a scenario",
    backend: "Backend",
    logBytes: "Log size",
    keysNow: "Keys",
    quota: "Origin quota used",
    download: "Download the log",
    reload: "Reload the page",
    reloadNote:
      "This log lives in your browser's origin-private filesystem, not in memory. Reload and the keys are still here — the engine replays the log on open.",
    aofNote:
      "The bytes below are the real append-only log, read straight back out of OPFS. It is RESP — the same wire format the server speaks.",
    aofEmpty: "The log is empty. Write a key and watch it grow.",
    channel: "Channel",
    message: "Message",
    publish: "Publish",
    openTab: "Open a second tab",
    tabNote:
      "Publish from either tab and both receive it. The bridge is a BroadcastChannel; the filtering happens inside the engine, not in JavaScript.",
    waiting: "Waiting for a message…",
    from: "from this tab",
    fromOther: "from another tab",
    booting: "Starting the engine…",
    failed: "The engine did not start",
    memOnly:
      "Persistent storage is unavailable here, so this session is in memory only. Everything else works; the log panel will stay empty.",
    scen: {
      session: "Session cache",
      rate: "Rate limiter",
      flags: "Feature flags",
      cart: "Shopping cart",
    },
    scenNote: {
      session: "Three sessions, each expiring on its own schedule. Watch the 8-second one go.",
      rate: "A counter per client, expiring on a window. INCR is atomic in the engine.",
      flags: "Config that outlives a reload, because it is on disk rather than in a variable.",
      cart: "A cart that survives the tab closing. This is the case localStorage is usually asked to do, and does badly.",
    },
  },
  zh: {
    keyspace: "键空间",
    ttl: "过期",
    storage: "持久化",
    pubsub: "跨标签页的发布订阅",
    key: "键",
    value: "值",
    expires: "过期时间",
    empty: "还没有数据 —— 跑一个场景、或者在下面写一个键。",
    noTtl: "不过期",
    keyPh: "键",
    valPh: "值",
    ttlPh: "存活秒数",
    write: "写入",
    del: "删除",
    flush: "清空",
    scenario: "载入一个场景",
    backend: "后端",
    logBytes: "日志大小",
    keysNow: "键数",
    quota: "已用配额",
    download: "下载这份日志",
    reload: "刷新页面",
    reloadNote:
      "这份日志躺在浏览器的 origin-private 文件系统里、不在内存里。刷新之后键还在 —— 引擎打开时会重放这份日志。",
    aofNote:
      "下面是真实的 append-only 日志字节、直接从 OPFS 读回来的。它是 RESP —— 跟服务器说的是同一种线上格式。",
    aofEmpty: "日志是空的。写一个键、看着它长出来。",
    channel: "频道",
    message: "消息",
    publish: "发布",
    openTab: "开第二个标签页",
    tabNote:
      "从任意一个标签页发布、两边都会收到。桥是 BroadcastChannel、而过滤发生在引擎内部、不在 JavaScript 里。",
    waiting: "等待消息……",
    from: "来自本标签页",
    fromOther: "来自另一个标签页",
    booting: "正在启动引擎……",
    failed: "引擎没能启动",
    memOnly:
      "这里拿不到持久化存储、所以本次会话只在内存里跑。其余功能照常、日志面板会一直是空的。",
    scen: {
      session: "会话缓存",
      rate: "限流器",
      flags: "功能开关",
      cart: "购物车",
    },
    scenNote: {
      session: "三个会话、各自按自己的节奏过期。盯着那个 8 秒的看。",
      rate: "每个客户端一个计数器、按窗口过期。INCR 在引擎内部是原子的。",
      flags: "能活过一次刷新的配置 —— 因为它在磁盘上、不在某个变量里。",
      cart: "关掉标签页也还在的购物车。这正是大家常拿 localStorage 干、但它干得很糟的事。",
    },
  },
  ja: {
    keyspace: "キー空間",
    ttl: "有効期限",
    storage: "永続化",
    pubsub: "タブをまたぐ Pub/Sub",
    key: "キー",
    value: "値",
    expires: "期限",
    empty: "まだ何も入っていない。シナリオを実行するか、下でキーを書き込む。",
    noTtl: "期限なし",
    keyPh: "キー",
    valPh: "値",
    ttlPh: "TTL(秒)",
    write: "書き込む",
    del: "削除",
    flush: "全消去",
    scenario: "シナリオを読み込む",
    backend: "バックエンド",
    logBytes: "ログサイズ",
    keysNow: "キー数",
    quota: "使用中のクォータ",
    download: "ログをダウンロード",
    reload: "ページを再読み込み",
    reloadNote:
      "このログはメモリではなく、ブラウザの origin-private ファイルシステムに置かれている。再読み込みしてもキーは残る —— エンジンが起動時にログを再生するからだ。",
    aofNote:
      "以下は本物の append-only ログのバイト列で、OPFS から直接読み戻したもの。形式は RESP —— サーバーが話すのと同じワイヤーフォーマットである。",
    aofEmpty: "ログは空。キーを書き込んで、伸びていく様子を見てほしい。",
    channel: "チャンネル",
    message: "メッセージ",
    publish: "発行",
    openTab: "二つ目のタブを開く",
    tabNote:
      "どちらのタブから発行しても、両方が受け取る。橋渡しは BroadcastChannel だが、フィルタリングは JavaScript ではなくエンジンの内部で行われる。",
    waiting: "メッセージを待っている……",
    from: "このタブから",
    fromOther: "別のタブから",
    booting: "エンジンを起動している……",
    failed: "エンジンが起動しなかった",
    memOnly:
      "ここでは永続ストレージが使えないため、このセッションはメモリ上のみで動く。他の機能はそのまま動作し、ログのパネルは空のままになる。",
    scen: {
      session: "セッションキャッシュ",
      rate: "レートリミッター",
      flags: "フィーチャーフラグ",
      cart: "ショッピングカート",
    },
    scenNote: {
      session: "三つのセッションが、それぞれ自分の時計で期限切れになる。8 秒のものに注目してほしい。",
      rate: "クライアントごとのカウンターが、ウィンドウ単位で期限切れになる。INCR はエンジン内部で原子的である。",
      flags: "再読み込みを生き延びる設定 —— 変数の中ではなく、ディスクの上にあるからだ。",
      cart: "タブを閉じても残るカート。localStorage によく任されて、うまくこなせない仕事がこれである。",
    },
  },
}[LANG];

// The scenarios. Each is a list of writes the buttons perform for real — there
// is no canned output anywhere on this page.
const SCENARIOS = {
  session: [
    ["session:7f3a", '{"user":"ada","role":"admin"}', 8],
    ["session:91bc", '{"user":"grace","role":"editor"}', 45],
    ["session:c204", '{"user":"alan","role":"viewer"}', 120],
  ],
  rate: [
    ["rate:203.0.113.7", "1", 30],
    ["rate:198.51.100.2", "14", 30],
    ["rate:192.0.2.44", "97", 30],
  ],
  flags: [
    ["flag:new-checkout", "on", 0],
    ["flag:dark-mode", "on", 0],
    ["flag:beta-search", "off", 0],
  ],
  cart: [
    ["cart:u881:items", '["sku-4410","sku-9982"]', 0],
    ["cart:u881:total", "8400", 0],
    ["cart:u881:currency", "JPY", 0],
  ],
};

const $ = (id) => document.getElementById(id);
const enc = new TextEncoder();
const dec = new TextDecoder();

let db = null;
let persistent = false;
const NAME = "playground";

// ── keyspace ────────────────────────────────────────────────────────────────
// Re-read from the engine on every paint. The page holds no shadow copy of the
// data, so what you see cannot drift from what is stored — including keys that
// vanished because their TTL came due while you were looking at them.

let lastSeen = new Map();

function paintKeyspace() {
  const names = db.keys("*", 200).sort();
  const rows = [];
  const now = new Map();

  for (const k of names) {
    const v = db.getText(k);
    if (v === undefined) continue; // expired between keys() and get()
    const ms = db.pttl(k);
    now.set(k, v);
    const fresh = !lastSeen.has(k);
    const changed = !fresh && lastSeen.get(k) !== v;
    rows.push(row(k, v, ms, fresh ? "fresh" : changed ? "changed" : ""));
  }

  const tb = $("ks-body");
  if (!rows.length) {
    tb.innerHTML = `<tr class="empty"><td colspan="3">${T.empty}</td></tr>`;
  } else {
    tb.innerHTML = rows.join("");
  }
  lastSeen = now;

  $("stat-keys").textContent = db.dbsize();
}

function row(k, v, ms, cls) {
  const val = v.length > 64 ? v.slice(0, 63) + "…" : v;
  let expiry;
  if (ms < 0) {
    expiry = `<span class="dim">${T.noTtl}</span>`;
  } else {
    const secs = ms / 1000;
    // Under ten seconds the bar is the point of the panel — it is the only
    // place on this page where you watch the engine take something away.
    const pct = Math.max(0, Math.min(100, (secs / 30) * 100));
    const urgent = secs <= 10 ? " urgent" : "";
    expiry =
      `<div class="ttl${urgent}"><span class="ttl-n">${secs.toFixed(1)}s</span>` +
      `<span class="ttl-bar"><i style="width:${pct}%"></i></span></div>`;
  }
  return (
    `<tr class="${cls}"><td class="k">${esc(k)}</td>` +
    `<td class="v">${esc(val)}</td><td class="t">${expiry}</td></tr>`
  );
}

function esc(s) {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

// ── durability ──────────────────────────────────────────────────────────────
// The log is read back out of OPFS by the page itself, with no help from the
// engine. That is the whole claim: the bytes are on your disk, and anything can
// go and look at them.

async function readLog() {
  if (!persistent) return null;
  try {
    const root = await navigator.storage.getDirectory();
    const dir = await root.getDirectoryHandle("kevy-wasm");
    const fh = await dir.getFileHandle(`${NAME}.aof`);
    const file = await fh.getFile();
    return new Uint8Array(await file.arrayBuffer());
  } catch {
    return null; // not written yet
  }
}

async function paintLog() {
  const bytes = await readLog();
  const n = bytes ? bytes.length : 0;
  $("stat-bytes").textContent = n ? `${n.toLocaleString()} B` : "0 B";

  const pre = $("aof");
  if (!n) {
    pre.innerHTML = `<span class="dim">${persistent ? T.aofEmpty : T.memOnly}</span>`;
    $("dl").disabled = true;
    return;
  }
  $("dl").disabled = false;

  // Show the tail — the newest writes are the ones you just made, and the head
  // of a replayed log is old news.
  const tail = bytes.slice(Math.max(0, n - 512));
  pre.textContent = dec
    .decode(tail)
    .replace(/\r\n/g, "␍␊\n")
    .trimStart();
  pre.scrollTop = pre.scrollHeight;

  if (navigator.storage?.estimate) {
    const { usage } = await navigator.storage.estimate();
    $("stat-quota").textContent = usage ? `${(usage / 1024).toFixed(0)} KB` : "—";
  }
}

// ── pub/sub ─────────────────────────────────────────────────────────────────
//
// The engine delivers a published message to every subscriber, including the
// one in the tab that published it — so the page must NOT also render its own
// send, or every message appears twice. And the callback carries no origin, so
// "which tab was this from" has to be worked out here: a publish leaves a claim
// ticket behind, and the first delivery that matches it claims it. Whatever is
// left unclaimed came from the other tab, which is the case the panel exists to
// show.

const unclaimed = new Set();
const ticket = (ch, msg) => `${ch}\u0000${msg}`;

function logMessage(text, channel, mine) {
  const feed = $("feed");
  if (feed.querySelector(".waiting")) feed.innerHTML = "";
  const li = document.createElement("li");
  li.className = mine ? "mine" : "other";
  li.innerHTML =
    `<span class="ch">${esc(channel)}</span>` +
    `<span class="msg">${esc(text)}</span>` +
    `<span class="src">${mine ? T.from : T.fromOther}</span>`;
  feed.prepend(li);
  while (feed.children.length > 8) feed.lastChild.remove();
}

// ── wiring ──────────────────────────────────────────────────────────────────

async function boot() {
  const base = document.body.dataset.pkg;
  const { open } = await import(`${base}/kevy.js`);

  try {
    db = await open({
      wasm: `${base}/kevy.wasm`,
      persist: { name: NAME },
      broadcast: true,
      tickMs: 100,
    });
    persistent = db.backend !== null;
  } catch (e) {
    // Persistence can be refused (private windows, some embedded webviews).
    // Falling back to memory is honest here: the panel says so out loud rather
    // than showing an empty log as though nothing had been written.
    db = await open({ wasm: `${base}/kevy.wasm`, persist: false, broadcast: true, tickMs: 100 });
    persistent = false;
  }

  $("boot").remove();
  $("app").hidden = false;
  $("stat-backend").textContent = persistent ? db.backend : "memory";
  if (!persistent) $("stat-backend").classList.add("warn");

  // Subscribe with a pattern so every channel on this page lands in the feed —
  // including messages published from the other tab.
  db.psubscribe("play:*", (payload, channel) => {
    const text = dec.decode(payload);
    logMessage(text, channel, unclaimed.delete(ticket(channel, text)));
  });

  paintKeyspace();
  await paintLog();

  // Repaint on a timer rather than on write, because TTLs expire without anyone
  // calling anything. The countdown IS the engine's clock, not a JS animation.
  setInterval(paintKeyspace, 100);
  setInterval(paintLog, 1000);

  bind();
}

function bind() {
  for (const b of document.querySelectorAll("[data-scenario]")) {
    b.addEventListener("click", () => {
      const s = b.dataset.scenario;
      for (const [k, v, ttl] of SCENARIOS[s]) {
        db.set(k, v, ttl ? { ttlMs: ttl * 1000 } : undefined);
      }
      $("scen-note").textContent = T.scenNote[s];
      paintKeyspace();
      paintLog();
    });
  }

  $("w").addEventListener("submit", (e) => {
    e.preventDefault();
    const k = $("wk").value.trim();
    if (!k) return;
    const ttl = parseFloat($("wt").value);
    db.set(k, $("wv").value, ttl > 0 ? { ttlMs: ttl * 1000 } : undefined);
    $("wk").value = $("wv").value = $("wt").value = "";
    paintKeyspace();
    paintLog();
  });

  $("ks-body").addEventListener("click", (e) => {
    const tr = e.target.closest("tr");
    if (!tr || tr.classList.contains("empty")) return;
    $("wk").value = tr.querySelector(".k").textContent;
    $("wv").value = tr.querySelector(".v").textContent;
    $("wk").focus();
  });

  $("flush").addEventListener("click", () => {
    db.flushall();
    lastSeen.clear();
    paintKeyspace();
    paintLog();
  });

  $("p").addEventListener("submit", (e) => {
    e.preventDefault();
    const ch = `play:${$("pc").value.trim() || "room"}`;
    const msg = $("pm").value;
    if (!msg) return;
    unclaimed.add(ticket(ch, msg));
    db.publish(ch, msg);
    $("pm").value = "";
  });

  $("dl").addEventListener("click", async () => {
    const bytes = await readLog();
    if (!bytes) return;
    const url = URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `${NAME}.aof`;
    a.click();
    URL.revokeObjectURL(url);
  });

  $("reload").addEventListener("click", () => location.reload());
  $("newtab").addEventListener("click", () => window.open(location.href, "_blank"));
}

boot().catch((e) => {
  $("boot").innerHTML =
    `<p class="fail">${T.failed}</p><pre>${esc(String(e))}</pre>`;
});
