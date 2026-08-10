#!/usr/bin/env python3
"""Render the playground shell in each language.

One implementation (assets/play.js, which carries its own string table), three
thin shells. Writing the page three times by hand would guarantee the three
drift apart, and the one that drifts is always the one nobody reads.

Run: python3 tools/gen_play.py
"""

import pathlib
import sys as _sys
_sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
from assetv import v as av

ROOT = pathlib.Path(__file__).resolve().parent.parent

L = {
    "en": {
        "dir": "",
        "lang": "en",
        "title": "Playground — kevy",
        "desc": "A real kevy engine, compiled to WebAssembly, running in this tab. Write keys, watch TTLs expire, read the append-only log off your own disk.",
        "eyebrow": "Playground",
        "h1": "The engine, in this tab",
        "lede": (
            "This is kevy-store — the same engine the server runs — compiled to "
            "WebAssembly. There is no backend. Turn off your network and the page "
            "keeps working, because the keys are in your browser's filesystem, not "
            "in ours."
        ),
        "booting": "Starting the engine…",
        "ks": "Keyspace",
        "ks_note": "Read back out of the engine every 100 ms. Click a row to edit it.",
        "write": "Write",
        "del": "Delete",
        "flush": "Flush all",
        "scen": "Scenarios",
        "scen_note": "Pick one. The buttons perform real writes — there is no canned output on this page.",
        "dur": "Durability",
        "pubsub": "Pub/Sub across tabs",
        "backend": "Backend",
        "bytes": "Log size",
        "keys": "Keys",
        "quota": "Origin quota used",
        "download": "Download the log",
        "reload": "Reload the page",
        "newtab": "Open a second tab",
        "s_session": "Session cache",
        "s_rate": "Rate limiter",
        "s_flags": "Feature flags",
        "s_cart": "Shopping cart",
        "aof_h": "The append-only log",
        "aof_note": "These bytes are read straight out of OPFS by this page, with no help from the engine. That is the claim: they are on your disk, and anything can go and look.",
        "reload_note": "Reload and the keys are still here. The engine replays this log when it opens — the same code path the server takes at boot.",
        "tab_note": "Publish from either tab and both receive it. The bridge is a BroadcastChannel; the channel filtering happens inside the engine, not in JavaScript.",
        "k_ph": "key",
        "v_ph": "value",
        "t_ph": "ttl (s)",
        "c_ph": "room",
        "m_ph": "message",
        "publish": "Publish",
        "waiting": "Waiting for a message…",
        "nav_docs": "Docs",
        "nav_cmds": "Commands",
        "nav_bench": "Benchmarks",
        "nav_play": "Playground",
    },
    "zh": {
        "dir": "zh/",
        "lang": "zh-Hans",
        "title": "Playground —— kevy",
        "desc": "一个真的 kevy 引擎，编译成 WebAssembly，跑在这个标签页里。写键、看 TTL 过期、把 append-only 日志从你自己的磁盘上读出来。",
        "eyebrow": "Playground",
        "h1": "引擎，就在这个标签页里",
        "lede": (
            "这是 kevy-store —— 跟服务器上跑的是同一个引擎 —— 编译成了 WebAssembly。"
            "没有后端。把网断掉，这个页面照样工作，因为键在你浏览器的文件系统里，不在我们的。"
        ),
        "booting": "正在启动引擎……",
        "ks": "键空间",
        "ks_note": "每 100 毫秒从引擎里重新读一遍。点一行可以编辑它。",
        "write": "写入",
        "del": "删除",
        "flush": "清空",
        "scen": "场景",
        "scen_note": "挑一个。这些按钮做的是真的写入 —— 这个页面上没有任何预录好的输出。",
        "dur": "持久化",
        "pubsub": "跨标签页的发布订阅",
        "backend": "后端",
        "bytes": "日志大小",
        "keys": "键数",
        "quota": "已用配额",
        "download": "下载这份日志",
        "reload": "刷新页面",
        "newtab": "开第二个标签页",
        "s_session": "会话缓存",
        "s_rate": "限流器",
        "s_flags": "功能开关",
        "s_cart": "购物车",
        "aof_h": "append-only 日志",
        "aof_note": "这些字节是这个页面直接从 OPFS 读出来的，没有经过引擎。这就是那个主张：它们在你的磁盘上，任何东西都可以去看。",
        "reload_note": "刷新之后键还在。引擎打开时会重放这份日志 —— 跟服务器启动时走的是同一条代码路径。",
        "tab_note": "从任意一个标签页发布，两边都会收到。桥是 BroadcastChannel，但频道的过滤发生在引擎内部，不在 JavaScript 里。",
        "k_ph": "键",
        "v_ph": "值",
        "t_ph": "存活秒数",
        "c_ph": "房间",
        "m_ph": "消息",
        "publish": "发布",
        "waiting": "等待消息……",
        "nav_docs": "文档",
        "nav_cmds": "命令",
        "nav_bench": "基准",
        "nav_play": "Playground",
    },
    "ja": {
        "dir": "ja/",
        "lang": "ja",
        "title": "Playground —— kevy",
        "desc": "本物の kevy エンジンを WebAssembly にコンパイルして、このタブで動かしている。キーを書き、TTL が切れるのを眺め、append-only ログを自分のディスクから読み出せる。",
        "eyebrow": "Playground",
        "h1": "エンジンが、このタブの中にある",
        "lede": (
            "これは kevy-store —— サーバーで動いているのと同じエンジン —— を WebAssembly に"
            "コンパイルしたものである。バックエンドは無い。ネットワークを切ってもページは動き続ける。"
            "キーはこちらのファイルシステムではなく、あなたのブラウザのファイルシステムにあるからだ。"
        ),
        "booting": "エンジンを起動している……",
        "ks": "キー空間",
        "ks_note": "100 ミリ秒ごとにエンジンから読み直している。行をクリックすると編集できる。",
        "write": "書き込む",
        "del": "削除",
        "flush": "全消去",
        "scen": "シナリオ",
        "scen_note": "一つ選んでほしい。ボタンは本物の書き込みを行う —— このページに作り置きの出力は一つも無い。",
        "dur": "永続化",
        "pubsub": "タブをまたぐ Pub/Sub",
        "backend": "バックエンド",
        "bytes": "ログサイズ",
        "keys": "キー数",
        "quota": "使用中のクォータ",
        "download": "ログをダウンロード",
        "reload": "ページを再読み込み",
        "newtab": "二つ目のタブを開く",
        "s_session": "セッションキャッシュ",
        "s_rate": "レートリミッター",
        "s_flags": "フィーチャーフラグ",
        "s_cart": "ショッピングカート",
        "aof_h": "append-only ログ",
        "aof_note": "以下のバイト列は、エンジンの助けを借りずにこのページが OPFS から直接読み出したものである。主張はそこにある —— これらはあなたのディスク上にあり、誰でも見に行ける。",
        "reload_note": "再読み込みしてもキーは残っている。エンジンは起動時にこのログを再生する —— サーバーが立ち上がるときと同じコードパスである。",
        "tab_note": "どちらのタブから発行しても、両方が受け取る。橋渡しは BroadcastChannel だが、チャンネルのフィルタリングは JavaScript ではなくエンジン内部で行われる。",
        "k_ph": "キー",
        "v_ph": "値",
        "t_ph": "TTL(秒)",
        "c_ph": "ルーム",
        "m_ph": "メッセージ",
        "publish": "発行",
        "waiting": "メッセージを待っている……",
        "nav_docs": "ドキュメント",
        "nav_cmds": "コマンド",
        "nav_bench": "ベンチマーク",
        "nav_play": "Playground",
    },
}

TPL = """<!doctype html>
<html lang="{lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<link rel="stylesheet" href="{up}assets/{css_kevy}">
<link rel="stylesheet" href="{up}assets/{css_play}">
<script>
  try {{
    var t = localStorage.getItem("kevy-theme");
    if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  }} catch (e) {{}}
</script>
</head>
<body data-pkg="{up}demo/pkg">
<a class="skip" href="#main">Skip to content</a>
<header class="mast">
  <div class="mast-in">
    <a class="brand" href="{up}{dir}">kevy<span class="v">5.0</span></a>
    <nav class="nav">
      <a href="{up}{dir}docs/">{nav_docs}</a>
      <a href="{up}{dir}docs/commands/">{nav_cmds}</a>
      <a href="{up}{dir}benchmarks/">{nav_bench}</a>
      <a href="{up}{dir}play/" aria-current="page">{nav_play}</a>
      <a href="https://github.com/goliajp/kevy">GitHub</a>
    </nav>
    <div class="mast-right">
      <nav class="lang" aria-label="Language">
        <a href="{up}play/"{en_cur}>EN</a>
        <a href="{up}zh/play/"{zh_cur}>中文</a>
        <a href="{up}ja/play/"{ja_cur}>日本語</a>
      </nav>
      <button class="icon-btn" id="theme" type="button" aria-label="Toggle theme">
        <svg class="sun" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="4.2"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M19.1 4.9l-1.4 1.4M6.3 17.7l-1.4 1.4"/></svg>
        <svg class="moon" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 14.5A8.2 8.2 0 0 1 9.5 4a8.3 8.3 0 1 0 10.5 10.5z"/></svg>
      </button>
    </div>
  </div>
</header>

<main id="main" class="page">
  <div class="band">
    <p class="eyebrow">{eyebrow}</p>
    <h1>{h1}</h1>
    <p class="lede">{lede}</p>
  </div>

  <div id="boot" class="boot"><span class="spin" aria-hidden="true"></span>{booting}</div>

  <div id="app" class="grid" hidden>

    <!-- keyspace ------------------------------------------------------- -->
    <section class="panel span2">
      <div class="panel-h">
        <h2>{ks}</h2>
        <p class="hint">{ks_note}</p>
      </div>
      <div class="ks-wrap">
        <table class="ks">
          <thead><tr><th>{k_ph}</th><th>{v_ph}</th><th>{t_ph}</th></tr></thead>
          <tbody id="ks-body"></tbody>
        </table>
      </div>
      <form id="w" class="wform">
        <input id="wk" placeholder="{k_ph}" aria-label="{k_ph}" autocomplete="off" spellcheck="false">
        <input id="wv" placeholder="{v_ph}" aria-label="{v_ph}" autocomplete="off" spellcheck="false">
        <input id="wt" placeholder="{t_ph}" aria-label="{t_ph}" type="number" min="0" step="1" inputmode="numeric">
        <button type="submit" class="btn primary">{write}</button>
        <button type="button" class="btn" id="flush">{flush}</button>
      </form>
    </section>

    <!-- scenarios ------------------------------------------------------ -->
    <section class="panel">
      <div class="panel-h">
        <h2>{scen}</h2>
        <p class="hint">{scen_note}</p>
      </div>
      <div class="scen">
        <button class="btn" data-scenario="session">{s_session}</button>
        <button class="btn" data-scenario="rate">{s_rate}</button>
        <button class="btn" data-scenario="flags">{s_flags}</button>
        <button class="btn" data-scenario="cart">{s_cart}</button>
      </div>
      <p id="scen-note" class="scen-note"></p>
    </section>

    <!-- durability ----------------------------------------------------- -->
    <section class="panel">
      <div class="panel-h"><h2>{dur}</h2></div>
      <dl class="stats">
        <div><dt>{backend}</dt><dd id="stat-backend" class="mono">—</dd></div>
        <div><dt>{keys}</dt><dd id="stat-keys" class="mono">0</dd></div>
        <div><dt>{bytes}</dt><dd id="stat-bytes" class="mono">0 B</dd></div>
        <div><dt>{quota}</dt><dd id="stat-quota" class="mono">—</dd></div>
      </dl>
      <p class="hint">{reload_note}</p>
      <div class="row">
        <button class="btn" id="reload">{reload}</button>
        <button class="btn" id="dl" disabled>{download}</button>
      </div>
    </section>

    <!-- the log -------------------------------------------------------- -->
    <section class="panel span2">
      <div class="panel-h">
        <h2>{aof_h}</h2>
        <p class="hint">{aof_note}</p>
      </div>
      <pre id="aof" class="aof" aria-live="polite"></pre>
    </section>

    <!-- pub/sub -------------------------------------------------------- -->
    <section class="panel span2">
      <div class="panel-h">
        <h2>{pubsub}</h2>
        <p class="hint">{tab_note}</p>
      </div>
      <form id="p" class="wform">
        <input id="pc" placeholder="{c_ph}" aria-label="{channel_l}" autocomplete="off" spellcheck="false" value="room">
        <input id="pm" placeholder="{m_ph}" aria-label="{m_ph}" autocomplete="off">
        <button type="submit" class="btn primary">{publish}</button>
        <button type="button" class="btn" id="newtab">{newtab}</button>
      </form>
      <ul id="feed" class="feed"><li class="waiting">{waiting}</li></ul>
    </section>

  </div>
</main>

<script type="module" src="{up}assets/{js_play}"></script>
<script>
  document.getElementById("theme").addEventListener("click", function () {{
    var r = document.documentElement;
    r.dataset.theme = r.dataset.theme === "dark" ? "light" : "dark";
    try {{ localStorage.setItem("kevy-theme", r.dataset.theme); }} catch (e) {{}}
  }});
</script>
</body>
</html>
"""


def main():
    for code, v in L.items():
        up = "../" * (1 if code == "en" else 2)
        out = ROOT / "site" / v["dir"] / "play" / "index.html"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(
            TPL.format(
                css_kevy=av('kevy.css'),
                css_play=av('play.css'),
                js_play=av('play.js'),
                up=up,
                channel_l=v["c_ph"],
                en_cur=' aria-current="page"' if code == "en" else "",
                zh_cur=' aria-current="page"' if code == "zh" else "",
                ja_cur=' aria-current="page"' if code == "ja" else "",
                **v,
            ),
            encoding="utf-8",
        )
        print(f"  wrote {out.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
