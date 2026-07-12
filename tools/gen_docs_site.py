#!/usr/bin/env python3
"""Render docs/*.md into the site, in three languages.

The markdown files stay the canonical source: they are what GitHub shows, what
`cargo doc` links to, and what a contributor edits. This turns them into pages
without asking anyone to maintain a second copy.

The grouping below is the one thing written by hand, because it is the one thing
a machine cannot infer: which of these twenty-four documents a person needs
first, and which they will only ever reach through a search box.

Run: python3 tools/gen_docs_site.py [--check]
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from md import inline, render  # noqa: E402

SRC = {"en": ROOT / "docs", "zh": ROOT / "docs/zh", "ja": ROOT / "docs/ja"}
DIRS = {"en": "", "zh": "zh/", "ja": "ja/"}
HTML_LANG = {"en": "en", "zh": "zh-Hans", "ja": "ja"}

# Not rendered: verb-reference is superseded by the command pages (same table,
# better shape), and UPGRADING belongs next to the code.
EXCLUDE = {"verb-reference"}

# The IA. Ordered by what a reader needs first, not alphabetically.
SECTIONS = [
    ("start", {"en": "Getting started", "zh": "上手", "ja": "はじめに"},
     ["designing-on-kevy", "cookbook", "persistence", "tuning"]),
    ("deploy", {"en": "Running it", "zh": "运行", "ja": "運用"},
     ["replication", "availability", "cluster", "accept-shards", "uds", "async"]),
    ("data", {"en": "Working with data", "zh": "数据", "ja": "データ"},
     ["indexes", "vector-search", "text-search", "views", "cdc", "pubsub", "lua"]),
    ("embed", {"en": "Embedding it", "zh": "嵌入", "ja": "組み込み"},
     ["wasm", "embedded-listener", "iot"]),
    ("ref", {"en": "Reference", "zh": "参考", "ja": "リファレンス"},
     ["error-replies", "rds-workloads", "migration", "UPGRADING"]),
]

# One line each, for the hub. Written here rather than scraped from the file,
# because a document's first paragraph is written to be read second.
BLURB = {
    "en": {
        "designing-on-kevy": "How to think in keys when you are used to thinking in tables.",
        "cookbook": "Recipes that run: sessions, rate limits, queues, leaderboards, feeds.",
        "persistence": "The append-only log, snapshots, and what survives a kill -9.",
        "tuning": "Shards, connections, and the two flags that actually matter.",
        "replication": "One primary, N replicas, and the offset that tells you the truth.",
        "availability": "Failover, epochs, and the writes we will not promise to keep.",
        "cluster": "Why there is no cluster, and what to do instead.",
        "accept-shards": "The flag that bought 10.6% by giving fewer cores the listener.",
        "uds": "Unix sockets: no TCP, no loopback, no kernel copy.",
        "async": "Pipelining, and why the engine has no async runtime.",
        "indexes": "Secondary indexes: how they build, and what they cost.",
        "vector-search": "KNN over your keyspace. No embedding model included.",
        "text-search": "BM25 full-text, and where it stops.",
        "views": "Materialised views, kept fresh by the write path.",
        "cdc": "A change feed you can tail from another process.",
        "pubsub": "Channels, patterns, and what happens to a slow subscriber.",
        "lua": "EVAL, on an interpreter we wrote.",
        "wasm": "151 KB in a browser tab, persisted to OPFS.",
        "embedded-listener": "Embed the engine and still speak RESP on a socket.",
        "iot": "no_std, no allocator, no operating system.",
        "error-replies": "Every error string, and the contract it is part of.",
        "rds-workloads": "What a relational workload costs here, honestly.",
        "migration": "Moving in from Redis, and back out again.",
        "UPGRADING": "3.x to 4.0: what breaks, and why.",
    },
    "zh": {
        "designing-on-kevy": "习惯了想表，现在要学着想键。",
        "cookbook": "能直接跑的配方：会话、限流、队列、排行榜、信息流。",
        "persistence": "append-only 日志、快照，以及 kill -9 之后还剩下什么。",
        "tuning": "shard、连接数，以及真正有用的那两个开关。",
        "replication": "一主 N 从，还有那个不会骗人的 offset。",
        "availability": "failover、epoch，以及我们不承诺保住的那些写。",
        "cluster": "为什么没有 cluster，以及该怎么办。",
        "accept-shards": "一个开关，把 listener 交给更少的核，换来 10.6%。",
        "uds": "Unix socket：没有 TCP、没有回环、没有内核拷贝。",
        "async": "pipelining，以及这个引擎为什么没有 async runtime。",
        "indexes": "二级索引：怎么建起来的，代价是多少。",
        "vector-search": "在你的键空间上做 KNN。不含 embedding 模型。",
        "text-search": "BM25 全文检索，以及它到哪儿为止。",
        "views": "物化视图，由写路径顺手保持新鲜。",
        "cdc": "一条变更流，别的进程可以 tail 它。",
        "pubsub": "频道、模式，以及订阅者跟不上时会发生什么。",
        "lua": "EVAL —— 跑在我们自己写的解释器上。",
        "wasm": "浏览器标签页里的 151 KB，持久化到 OPFS。",
        "embedded-listener": "嵌进去，同时还能在 socket 上讲 RESP。",
        "iot": "no_std、没有分配器、没有操作系统。",
        "error-replies": "每一条错误字符串，以及它属于哪份契约。",
        "rds-workloads": "关系型负载在这里到底要付多少钱 —— 照实说。",
        "migration": "从 Redis 搬进来，以及再搬出去。",
        "UPGRADING": "3.x 到 4.0：什么会坏，为什么。",
    },
    "ja": {
        "designing-on-kevy": "テーブルで考えることに慣れた頭で、キーで考え直す。",
        "cookbook": "そのまま動くレシピ —— セッション、レート制限、キュー、ランキング、フィード。",
        "persistence": "append-only ログ、スナップショット、そして kill -9 の後に何が残るか。",
        "tuning": "shard、コネクション、そして本当に効く二つのフラグ。",
        "replication": "一つの primary と N 個の replica、そして嘘をつかない offset。",
        "availability": "failover、epoch、そして我々が守ると約束しない書き込み。",
        "cluster": "cluster が無い理由と、代わりに何をするか。",
        "accept-shards": "listener を持つ core を減らして 10.6% を買ったフラグ。",
        "uds": "Unix ドメインソケット —— TCP も loopback も kernel コピーも無い。",
        "async": "pipelining と、このエンジンに async runtime が無い理由。",
        "indexes": "セカンダリインデックス —— どう構築され、何を払うのか。",
        "vector-search": "キー空間の上での KNN。embedding モデルは含まない。",
        "text-search": "BM25 全文検索と、その限界。",
        "views": "マテリアライズドビュー。書き込みパスが鮮度を保つ。",
        "cdc": "別プロセスから tail できる変更フィード。",
        "pubsub": "チャンネル、パターン、そして遅い購読者に何が起きるか。",
        "lua": "EVAL —— 自前で書いたインタープリタの上で。",
        "wasm": "ブラウザのタブに 151 KB、OPFS へ永続化。",
        "embedded-listener": "組み込みつつ、socket では RESP を話す。",
        "iot": "no_std、アロケータ無し、OS 無し。",
        "error-replies": "すべてのエラー文字列と、それが属する契約。",
        "rds-workloads": "リレーショナルなワークロードがここで払う代価を、正直に。",
        "migration": "Redis から移ってくる道と、出ていく道。",
        "UPGRADING": "3.x から 4.0 へ —— 何が壊れ、なぜ壊れるのか。",
    },
}

CSS_HREF = "assets/docs.css"


def title_of(md, fallback):
    m = re.search(r"^#\s+(.*)", md, re.M)
    return re.sub(r"[`*]", "", m.group(1)).strip() if m else fallback


def blurb(slug, lang, titles):
    """The one-liner for the hub, in the reader's language.

    Falls back to English rather than to nothing: a hub card whose title is
    Chinese and whose blurb is English is at least honest about which half has
    been done. A blank one just looks broken.
    """
    return BLURB.get(lang, {}).get(slug) or BLURB["en"].get(slug) or titles.get(slug, slug)


def shell(lang, slug, title, desc, body, toc, nav, depth, have=None):
    up = "../" * depth
    d = DIRS[lang]

    # The language switcher only offers a language that actually HAS this page.
    # Twelve of the twenty-four documents have no Chinese version yet and
    # thirteen have no Japanese one; a switcher that linked to them anyway would
    # be handing the reader a 404 in their own language, which is a worse
    # welcome than not offering it. Where the translation is missing, the link
    # goes to that language's documentation hub instead, and says so by not
    # pretending.
    def lang_link(c, label):
        if slug and have is not None and slug not in have.get(c, ()):
            href = f"{up}{DIRS[c]}docs/"
            return f'<a href="{href}" class="untranslated" title="not translated yet">{label}</a>'
        href = f'{up}{DIRS[c]}docs/{slug + "/" if slug else ""}'
        cur = ' aria-current="page"' if c == lang else ""
        return f'<a href="{href}"{cur}>{label}</a>'

    lang_row = "".join(
        lang_link(c, label)
        for c, label in (("en", "EN"), ("zh", "中文"), ("ja", "日本語"))
    )
    nav_l = {"en": ("Docs", "Commands", "Benchmarks", "Playground"),
             "zh": ("文档", "命令", "基准", "Playground"),
             "ja": ("ドキュメント", "コマンド", "ベンチマーク", "Playground")}[lang]
    toc_html = ""
    if len(toc) > 1:
        items = "".join(
            f'<li class="l{lvl}"><a href="#{s}">{t}</a></li>' for lvl, s, t in toc
        )
        on_page = {"en": "On this page", "zh": "本页内容", "ja": "このページの内容"}[lang]
        toc_html = f'<aside class="toc"><p class="toc-h">{on_page}</p><ul>{items}</ul></aside>'

    return f"""<!doctype html>
<html lang="{HTML_LANG[lang]}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<link rel="stylesheet" href="{up}assets/kevy.css">
<link rel="stylesheet" href="{up}assets/docs.css">
<script>
  try {{
    var t = localStorage.getItem("kevy-theme");
    if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  }} catch (e) {{}}
</script>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="mast">
  <div class="mast-in">
    <a class="brand" href="{up}{d}">kevy<span class="v">4.0</span></a>
    <nav class="nav">
      <a href="{up}{d}docs/" aria-current="page">{nav_l[0]}</a>
      <a href="{up}{d}docs/commands/">{nav_l[1]}</a>
      <a href="{up}{d}benchmarks/">{nav_l[2]}</a>
      <a href="{up}{d}play/">{nav_l[3]}</a>
    </nav>
    <div class="mast-right">
      <nav class="lang" aria-label="Language">{lang_row}</nav>
      <button class="icon-btn" id="theme" type="button" aria-label="Toggle theme">
        <svg class="sun" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="4.2"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M19.1 4.9l-1.4 1.4M6.3 17.7l-1.4 1.4"/></svg>
        <svg class="moon" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 14.5A8.2 8.2 0 0 1 9.5 4a8.3 8.3 0 1 0 10.5 10.5z"/></svg>
      </button>
    </div>
  </div>
</header>

<div class="doc-shell">
  <aside class="side">{nav}</aside>
  <main id="main" class="doc">{body}</main>
  {toc_html}
</div>

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


def build(lang, present, titles, have):
    """Return {path: html} for one language. `have` maps lang -> set of slugs."""
    pages = {}
    d = DIRS[lang]

    # sidebar, shared by every page in the language
    def sidebar(depth, here):
        up = "../" * depth
        out = []
        for _, names, slugs in SECTIONS:
            live = [s for s in slugs if s in present]
            if not live:
                continue
            items = "".join(
                f'<li><a href="{up}{d}docs/{s}/"'
                f'{" aria-current=\"page\"" if s == here else ""}>{titles[s]}</a></li>'
                for s in live
            )
            out.append(f'<p class="side-h">{names[lang]}</p><ul>{items}</ul>')
        cmds = {"en": "All 184 commands", "zh": "全部 184 个命令", "ja": "184 コマンド全て"}[lang]
        out.append(f'<p class="side-h">&nbsp;</p><ul><li><a href="{up}{d}docs/commands/">{cmds}</a></li></ul>')
        return "".join(out)

    # ── hub ──
    depth = 1 + (0 if lang == "en" else 1)
    up = "../" * depth
    hub_h1 = {"en": "Documentation", "zh": "文档", "ja": "ドキュメント"}[lang]
    hub_lede = {
        "en": "Twenty-four documents, ordered by what you need first. The command reference is generated from the engine's own verb table and lives <a href=\"commands/\">here</a>.",
        "zh": "二十四篇文档，按你先需要什么排序。命令参考是从引擎自己的动词表生成的，在<a href=\"commands/\">这里</a>。",
        "ja": "二十四本のドキュメントを、先に必要になる順に並べてある。コマンドリファレンスはエンジン自身の動詞テーブルから生成しており、<a href=\"commands/\">こちら</a>にある。",
    }[lang]

    secs = []
    for _, names, slugs in SECTIONS:
        live = [s for s in slugs if s in present]
        if not live:
            continue
        cards = "".join(
            f'<a class="card" href="{s}/"><h3>{titles[s]}</h3>'
            f'<p>{inline(blurb(s, lang, titles))}</p></a>'
            for s in live
        )
        secs.append(f'<section class="hub-sec"><h2>{names[lang]}</h2><div class="cards">{cards}</div></section>')

    hub = (
        f'<div class="doc-head"><h1>{hub_h1}</h1><p class="lede">{hub_lede}</p></div>'
        + "".join(secs)
    )
    pages[ROOT / "site" / d / "docs" / "index.html"] = shell(
        lang, "", hub_h1 + " — kevy", re.sub(r"<[^>]+>", "", hub_lede),
        hub, [], sidebar(depth, ""), depth, have,
    )

    # ── one page per document ──
    depth = 2 + (0 if lang == "en" else 1)
    for s in present:
        text = (SRC[lang] / f"{s}.md").read_text(encoding="utf-8")
        # A link to ../other.md inside the markdown becomes ../other/ on the site.
        def relink(href):
            # docs/foo.md, ./foo.md and ../foo.md all point at the same document;
            # on the site every document is a directory, so they all become
            # ../foo/. verb-reference has no page of its own — the generated
            # command reference is the same table in a better shape.
            # A link out of docs/ into the repository — bench/perfgate.sh,
            # crates/…, the README — has no page on the site. It goes to GitHub,
            # where the file actually is. This catches scripts and sources, not
            # just markdown: the docs cite `../bench/iotgate.sh` and a reader who
            # clicks that deserves to land on it.
            out = re.match(r"^(?:\.{1,2}/)+((?:bench|crates|tools|examples|\.github)/.*|[A-Z]+\.md)$", href)
            if out:
                return f"https://github.com/goliajp/kevy/blob/main/{out.group(1)}"
            m = re.match(r"^(?:\.{1,2}/)*([\w.-]+)\.md(#.*)?$", href)
            if not m:
                return href
            name, frag = m.group(1), m.group(2) or ""
            if name == "verb-reference":
                return f"../commands/{frag}"
            if name not in present:
                # A Japanese document citing a sibling that has no Japanese
                # version yet. Send the reader to the English one rather than to
                # a 404 — an English page they can read beats a Japanese page
                # that is not there.
                return f"{'../' * (2 if lang == 'en' else 3)}docs/{name}/{frag}"
            return f"../{name}/{frag}"

        body, toc = render(text, relink)
        # The <h1> the markdown carries becomes the page's own head.
        body = re.sub(r"^<h1[^>]*>(.*?)</h1>", r'<div class="doc-head"><h1>\1</h1></div>', body, count=1)
        desc = re.sub(r"<[^>]+>", "", re.search(r"<p>(.*?)</p>", body, re.S).group(1))[:180] if "<p>" in body else titles[s]
        pages[ROOT / "site" / d / "docs" / s / "index.html"] = shell(
            lang, s, f"{titles[s]} — kevy", desc, body, toc, sidebar(depth, s), depth, have,
        )
    return pages


def main():
    check = "--check" in sys.argv
    want = {}
    have = {
        lang: {p.stem for p in SRC[lang].glob("*.md") if p.stem not in EXCLUDE}
        for lang in ("en", "zh", "ja")
    }
    for lang in ("en", "zh", "ja"):
        present = sorted(have[lang])
        if not present:
            continue
        src = SRC[lang]
        titles = {s: title_of((src / f"{s}.md").read_text(encoding="utf-8"), s) for s in present}
        want.update(build(lang, present, titles, have))

    if check:
        stale = [p for p, t in want.items()
                 if not p.exists() or p.read_text(encoding="utf-8") != t]
        if stale:
            print(f"STALE: {len(stale)} doc pages differ from docs/*.md")
            for p in stale[:5]:
                print(f"  {p.relative_to(ROOT)}")
            print("Regenerate with `python3 tools/gen_docs_site.py`.")
            return 1
        print(f"ok: {len(want)} doc pages match docs/*.md")
        return 0

    for p, t in want.items():
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(t, encoding="utf-8")
    print(f"  wrote {len(want)} doc pages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
