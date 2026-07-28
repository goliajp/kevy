#!/usr/bin/env python3
"""Render the command reference from site/data/commands.json.

One page per command, in every language — the shape Redis and rustdoc use,
because a reader who lands on `/docs/commands/SET/` from a search engine or a
teammate's link should get the whole page, not an anchor into a wall.

The data comes from the engine's own verb table (`cargo run -p kevy --bin
gen_docs .` writes commands.json), so the pages cannot drift from what the
server actually does. CI's gen_docs --check holds that.

Run: python3 tools/gen_command_pages.py
"""

import json
import pathlib
import re
import shutil
import sys
import sys as _sys
_sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
from assetv import v as av

ROOT = pathlib.Path(__file__).resolve().parent.parent
DATA = json.loads((ROOT / "site/data/commands.json").read_text(encoding="utf-8"))

# summary / complexity / compat, written natively in each language rather than
# translated. Absent for English (the table itself IS the English). A command
# missing from a locale falls back to English, and gen reports how many did.
I18N = {}
for _lang in ("zh", "ja"):
    _p = ROOT / "site/data" / f"commands.{_lang}.json"
    I18N[_lang] = json.loads(_p.read_text(encoding="utf-8")) if _p.exists() else {}


def field(c, lang, key):
    """The localized text for a command field, falling back to English."""
    return I18N.get(lang, {}).get(c["name"], {}).get(key) or c[key]

# ── languages ────────────────────────────────────────────────────────────────
# The page chrome is localized. The `complexity` and `compat` strings are not:
# they are normative statements derived from the engine's source, and a
# paraphrase of "LINDEX is O(1) because the list is a ring buffer, not a quicklist"
# is a place for drift to hide. They are rendered verbatim, and each locale says
# so rather than pretending otherwise.
LANGS = {
    "en": {
        "dir": "",
        "html_lang": "en",
        "commands": "Commands",
        "nav_choose": "Should I use it?",
        "nav_docs": "Docs",
        "nav_bench": "Benchmarks",
        "search_ph": "Search docs…",
        "title_suffix": "kevy command reference",
        "search": "Filter commands…",
        "group": "Group",
        "since": "Since",
        "arity": "Arity",
        "flags": "Flags",
        "complexity": "Complexity",
        "compat": "Redis compatibility",
        "syntax": "Syntax",
        "summary": "Summary",
        "see_also": "See also",
        "all_groups": "All groups",
        "all": "All",
        "full": "Compatible",
        "differs": "Differs",
        "kevy_only": "kevy only",
        "count": "{n} commands",
        "index_lede": (
            "Every verb the server will answer, straight from its own registry — "
            "the same table <code>COMMAND DOCS</code> reads. Two columns you will "
            "not find in Redis's reference: what the command <b>costs in this "
            "engine</b>, and <b>how it differs from Redis</b>."
        ),
        "compat_note_full": "Behaves as Redis does.",
        "compat_note_only": "kevy has this; Redis does not.",
        "back": "← All commands",
        "engine_note": (
            "Complexity and compatibility are read out of kevy's implementation, "
            "not copied from Redis's reference. Several genuinely differ."
        ),
    },
    "zh": {
        "dir": "zh/",
        "html_lang": "zh-Hans",
        "commands": "命令",
        "nav_choose": "该不该用",
        "nav_docs": "文档",
        "nav_bench": "基准",
        "search_ph": "搜索文档……",
        "title_suffix": "kevy 命令参考",
        "search": "筛选命令……",
        "group": "分组",
        "since": "起始版本",
        "arity": "参数个数",
        "flags": "标志",
        "complexity": "复杂度",
        "compat": "与 Redis 的兼容性",
        "syntax": "语法",
        "summary": "摘要",
        "see_also": "另见",
        "all_groups": "全部分组",
        "all": "全部",
        "full": "兼容",
        "differs": "有差异",
        "kevy_only": "kevy 独有",
        "count": "{n} 个命令",
        "index_lede": (
            "服务器会应答的每一个动词，直接来自它自己的注册表 —— "
            "也就是 <code>COMMAND DOCS</code> 读的同一张表。有两栏是 Redis 的参考文档里没有的："
            "这个命令在<b>本引擎里的真实代价</b>，以及它<b>与 Redis 的差异</b>。"
        ),
        "compat_note_full": "行为与 Redis 一致。",
        "compat_note_only": "kevy 有这个动词，Redis 没有。",
        "back": "← 返回全部命令",
        "engine_note": (
            "复杂度与兼容性是从 kevy 的实现里读出来的，不是抄 Redis 的文档。"
            "有几条确实不一样 —— 它们看起来像笔误，但不是。"
        ),
    },
    "ja": {
        "dir": "ja/",
        "html_lang": "ja",
        "commands": "コマンド",
        "nav_choose": "使うべきか",
        "nav_docs": "ドキュメント",
        "nav_bench": "ベンチマーク",
        "search_ph": "ドキュメントを検索……",
        "title_suffix": "kevy コマンドリファレンス",
        "search": "コマンドを絞り込む……",
        "group": "グループ",
        "since": "追加バージョン",
        "arity": "引数の数",
        "flags": "フラグ",
        "complexity": "計算量",
        "compat": "Redis との互換性",
        "syntax": "構文",
        "summary": "概要",
        "see_also": "関連",
        "all_groups": "すべてのグループ",
        "all": "すべて",
        "full": "互換",
        "differs": "差異あり",
        "kevy_only": "kevy 固有",
        "count": "{n} コマンド",
        "index_lede": (
            "サーバーが応答するすべての動詞を、サーバー自身のレジストリから直接 —— "
            "<code>COMMAND DOCS</code> が読むのと同じテーブルである。Redis のリファレンスには無い列が二つある。"
            "このエンジンでの<b>実際のコスト</b>と、<b>Redis との差異</b>である。"
        ),
        "compat_note_full": "Redis と同じ挙動。",
        "compat_note_only": "kevy にはあるが、Redis には無い。",
        "back": "← コマンド一覧へ",
        "engine_note": (
            "計算量と互換性は kevy の実装から読み取ったものであり、"
            "Redis のドキュメントを写したものではない。実際に異なるものがいくつかある。"
        ),
    },
}

GROUP_LABEL = {
    "string": {"en": "String", "zh": "字符串", "ja": "文字列"},
    "list": {"en": "List", "zh": "列表", "ja": "リスト"},
    "hash": {"en": "Hash", "zh": "哈希", "ja": "ハッシュ"},
    "set": {"en": "Set", "zh": "集合", "ja": "集合"},
    "zset": {"en": "Sorted set", "zh": "有序集合", "ja": "ソート済み集合"},
    "stream": {"en": "Stream", "zh": "流", "ja": "ストリーム"},
    "geo": {"en": "Geo", "zh": "地理空间", "ja": "地理空間"},
    "generic": {"en": "Generic", "zh": "通用键操作", "ja": "汎用キー操作"},
    "scan": {"en": "Keyspace scan", "zh": "键空间扫描", "ja": "キースペース走査"},
    "connection": {"en": "Connection", "zh": "连接", "ja": "接続"},
    "server": {"en": "Server", "zh": "服务器", "ja": "サーバー"},
    "tx": {"en": "Transactions", "zh": "事务", "ja": "トランザクション"},
    "script": {"en": "Scripting", "zh": "脚本", "ja": "スクリプト"},
    "pubsub": {"en": "Pub/Sub", "zh": "发布订阅", "ja": "Pub/Sub"},
    "replication": {"en": "Replication", "zh": "复制", "ja": "レプリケーション"},
    "index": {"en": "Indexes", "zh": "索引", "ja": "インデックス"},
    "table": {"en": "Tables", "zh": "表", "ja": "テーブル"},
    "view": {"en": "Views", "zh": "视图", "ja": "ビュー"},
    "feed": {"en": "Change feed", "zh": "变更流", "ja": "変更フィード"},
    "migration": {"en": "Migration", "zh": "迁移", "ja": "マイグレーション"},
}

ORDER = [
    "string", "generic", "scan", "list", "hash", "set", "zset", "stream", "geo",
    "connection", "server", "tx", "script", "pubsub", "replication",
    "index", "table", "view", "feed", "migration",
]


def esc(s):
    return (
        s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def brief(complexity):
    """The leading cost term, for the index table.

    A full complexity line runs to a paragraph — APPEND's explains why repeated
    appends are O(N^2). Printed whole in a table cell it pushed the compatibility
    and summary columns clean off the right-hand edge, where nobody found them.
    The index shows the term; the command's own page shows the reasoning.
    """
    head = re.split(r"\s+—\s+|\s+——\s*|;|。|、", complexity, maxsplit=1)[0].strip()
    return head if len(head) <= 40 else head[:38].rstrip() + "…"


def compat_kind(compat):
    if compat == "full":
        return "full"
    if compat.startswith("kevy-only"):
        return "only"
    return "differs"


def arity_text(n, lang):
    if n >= 0:
        return {"en": f"exactly {n}", "zh": f"恰好 {n} 个", "ja": f"ちょうど {n} 個"}[lang]
    return {"en": f"at least {abs(n)}", "zh": f"至少 {abs(n)} 个", "ja": f"{abs(n)} 個以上"}[lang]


def head(title, lang, depth, desc):
    L = LANGS[lang]
    up = "../" * depth
    return f"""<!doctype html>
<html lang="{L['html_lang']}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{esc(title)}</title>
<meta name="description" content="{esc(desc)}">
<link rel="stylesheet" href="{up}assets/{av('kevy.css')}">
<link rel="stylesheet" href="{up}assets/{av('docs.css')}">
<script src="{up}assets/{av('docsearch.js')}" defer></script>
<script>
  try {{
    var t = localStorage.getItem("kevy-theme");
    if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  }} catch (e) {{}}
</script>
</head>
<body>
"""


def mast(lang, depth, here):
    L = LANGS[lang]
    up = "../" * depth
    # Language switch keeps the reader on the same page.
    def other(code):
        d = LANGS[code]["dir"]
        return f"{up}{'../' * 0}{d}docs/commands/{here}" if here else f"{up}{d}docs/commands/"
    return f"""<a class="skip" href="#main">Skip to content</a>
<header class="mast">
  <div class="mast-in">
    <a class="brand" href="{up}{L['dir']}">kevy<span class="v">4.1</span></a>
    <nav class="nav">
      <a href="{up}{L['dir']}docs/">{esc(L['nav_docs'])}</a>
      <a href="{up}{L['dir']}docs/commands/" aria-current="page">{esc(L['commands'])}</a>
      <a href="{up}{L['dir']}benchmarks/">{esc(L['nav_bench'])}</a>
      <a href="{up}{L['dir']}play/">Playground</a>
      <a href="https://github.com/goliajp/kevy">GitHub</a>
    </nav>
    <div class="mast-right">
      <div id="docsearch" data-index="{up}{L['dir']}docs/search-index.json" data-root="{up}{L['dir']}">
        <input type="search" placeholder="{esc(L['search_ph'])}" aria-label="{esc(L['search_ph'])}" autocomplete="off" spellcheck="false">
        <span class="ds-key">/</span>
        <div class="ds-list"></div>
      </div>
      <nav class="lang" aria-label="Language">
        <a href="{up}docs/commands/{here}"{' aria-current="page"' if lang == 'en' else ''}>EN</a>
        <a href="{up}zh/docs/commands/{here}"{' aria-current="page"' if lang == 'zh' else ''}>中文</a>
        <a href="{up}ja/docs/commands/{here}"{' aria-current="page"' if lang == 'ja' else ''}>日本語</a>
      </nav>
      <button class="icon-btn" id="theme" type="button" aria-label="Toggle theme">
        <svg class="sun" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="4.2"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M19.1 4.9l-1.4 1.4M6.3 17.7l-1.4 1.4"/></svg>
        <svg class="moon" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 14.5A8.2 8.2 0 0 1 9.5 4a8.3 8.3 0 1 0 10.5 10.5z"/></svg>
      </button>
    </div>
  </div>
</header>
"""


THEME_JS = """<script>
  document.getElementById("theme").addEventListener("click", function () {
    var r = document.documentElement;
    r.dataset.theme = r.dataset.theme === "dark" ? "light" : "dark";
    try { localStorage.setItem("kevy-theme", r.dataset.theme); } catch (e) {}
  });
</script>
"""


def render_index(lang):
    L = LANGS[lang]
    cmds = DATA["commands"]
    groups = {}
    for c in cmds:
        groups.setdefault(c["group"], []).append(c)

    rows = []
    for g in ORDER:
        if g not in groups:
            continue
        for c in sorted(groups[g], key=lambda x: x["name"]):
            kind = compat_kind(c["compat"])
            badge = {"full": L["full"], "differs": L["differs"], "only": L["kevy_only"]}[kind]
            rows.append(
                f'<tr data-name="{esc(c["name"].lower())}" data-group="{esc(g)}" data-compat="{kind}">'
                f'<td class="k"><a href="{esc(c["name"])}/">{esc(c["name"])}</a></td>'
                f'<td class="g">{esc(GROUP_LABEL[g][lang])}</td>'
                f'<td class="cx mono">{esc(brief(field(c, lang, "complexity")))}</td>'
                f'<td><span class="badge {kind}">{esc(badge)}</span></td>'
                f'<td class="sum">{esc(field(c, lang, "summary"))}</td>'
                f"</tr>"
            )

    opts = "".join(
        f'<option value="{esc(g)}">{esc(GROUP_LABEL[g][lang])}</option>'
        for g in ORDER if g in groups
    )

    body = f"""<main id="main" class="page">
  <div class="band">
    <p class="eyebrow">{esc(L['commands'])}</p>
    <h1>{esc(L['count']).format(n=DATA['count'])}</h1>
    <p class="lede">{L['index_lede']}</p>

    <div class="filters">
      <input id="q" type="search" placeholder="{esc(L['search'])}" aria-label="{esc(L['search'])}" autocomplete="off">
      <select id="g" aria-label="{esc(L['group'])}">
        <option value="">{esc(L['all_groups'])}</option>{opts}
      </select>
      <select id="c" aria-label="{esc(L['compat'])}">
        <option value="">{esc(L['all'])} — {esc(L['compat'])}</option>
        <option value="full">{esc(L['full'])}</option>
        <option value="differs">{esc(L['differs'])}</option>
        <option value="only">{esc(L['kevy_only'])}</option>
      </select>
      <span id="n" class="hits"></span>
    </div>

    <div class="tbl">
      <table id="t">
        <thead><tr>
          <th>{esc(L['commands'])}</th><th>{esc(L['group'])}</th>
          <th>{esc(L['complexity'])}</th><th>{esc(L['compat'])}</th>
          <th>{esc(L['summary'])}</th>
        </tr></thead>
        <tbody>
{chr(10).join(rows)}
        </tbody>
      </table>
    </div>
  </div>
</main>

<script>
  // Three axes, the way Redis's reference does it: name, group, and — ours —
  // whether the verb behaves like Redis at all. No index, no library: 183 rows
  // filter faster than a keystroke.
  var q = document.getElementById("q"), g = document.getElementById("g"),
      c = document.getElementById("c"), n = document.getElementById("n"),
      rows = Array.prototype.slice.call(document.querySelectorAll("#t tbody tr"));
  function apply() {{
    var term = q.value.trim().toLowerCase(), grp = g.value, cmp = c.value, hits = 0;
    rows.forEach(function (r) {{
      var ok = (!term || r.dataset.name.indexOf(term) !== -1)
            && (!grp || r.dataset.group === grp)
            && (!cmp || r.dataset.compat === cmp);
      r.hidden = !ok;
      if (ok) hits++;
    }});
    n.textContent = hits + " / " + rows.length;
  }}
  [q, g, c].forEach(function (el) {{ el.addEventListener("input", apply); }});
  // `/` focuses the filter, the way every reference worth using does.
  document.addEventListener("keydown", function (e) {{
    if (e.key === "/" && document.activeElement !== q) {{ e.preventDefault(); q.focus(); }}
  }});
  apply();
</script>
{THEME_JS}</body>
</html>
"""
    # site/docs/commands/           -> up 2
    # site/{zh,ja}/docs/commands/    -> up 3
    d = 2 + (0 if lang == "en" else 1)
    return head(f"{LANGS[lang]['commands']} — {L['title_suffix']}", lang, d, L["index_lede"]) + mast(lang, d, "") + body


def render_command(c, lang, siblings):
    L = LANGS[lang]
    kind = compat_kind(c["compat"])  # the KIND always comes from the English
    badge = {"full": L["full"], "differs": L["differs"], "only": L["kevy_only"]}[kind]
    g = c["group"]

    loc_compat = field(c, lang, "compat")
    compat_body = loc_compat
    if lang != "en":
        # A native write-up already carries its own prefix («有差异：» / «差異あり。»).
        pass
    elif kind == "full":
        compat_body = L["compat_note_full"]
    elif kind == "only":
        # `kevy-only: nearest is …` — keep the analogue, it is the useful half.
        rest = c["compat"][len("kevy-only"):].lstrip(": ").strip()
        compat_body = (L["compat_note_only"] + (" " + rest if rest else ""))
    else:
        compat_body = c["compat"][len("differs:"):].strip()
    if lang != "en" and loc_compat != c["compat"]:
        compat_body = loc_compat

    sees = "".join(
        f'<a href="../{esc(s)}/">{esc(s)}</a>'
        for s in siblings if s != c["name"]
    )

    desc = f'{c["name"]} — {field(c, lang, "summary")} {field(c, lang, "complexity")}'
    body = f"""<main id="main" class="page">
  <article class="band ref">
    <nav class="crumb"><a href="../">{esc(L['commands'])}</a> <span>/</span> <span>{esc(GROUP_LABEL[g][lang])}</span> <span>/</span> <b>{esc(c['name'])}</b></nav>

    <h1 class="cmd">{esc(c['name'])}</h1>
    <p class="lede">{esc(field(c, lang, "summary"))}</p>

    <pre class="syntax"><code>{esc(c['syntax'])}</code></pre>

    <dl class="meta">
      <div><dt>{esc(L['group'])}</dt><dd>{esc(GROUP_LABEL[g][lang])}</dd></div>
      <div><dt>{esc(L['since'])}</dt><dd class="mono">{esc(c['since'])}</dd></div>
      <div><dt>{esc(L['arity'])}</dt><dd class="mono">{esc(arity_text(c['arity'], lang))}</dd></div>
      <div><dt>{esc(L['flags'])}</dt><dd>{" ".join(f'<code>{esc(f)}</code>' for f in c['flags'])}</dd></div>
    </dl>

    <section>
      <h2>{esc(L['complexity'])}</h2>
      <p class="cx-body"><code>{esc(field(c, lang, "complexity"))}</code></p>
      <p class="engine-note">{esc(L['engine_note'])}</p>
    </section>

    <section>
      <h2>{esc(L['compat'])}</h2>
      <div class="call {'note' if kind == 'full' else ('warn' if kind == 'only' else 'loss')}">
        <span class="h">{esc(badge)}</span>
        <p>{esc(compat_body)}</p>
      </div>
    </section>

    <section class="siblings">
      <h2>{esc(L['see_also'])}</h2>
      <p class="sees">{sees}</p>
    </section>

    <p class="back"><a href="../">{esc(L['back'])}</a></p>
  </article>
</main>
{THEME_JS}</body>
</html>
"""
    # site/docs/commands/SET/          -> up 3
    # site/{zh,ja}/docs/commands/SET/  -> up 4
    d = 3 + (0 if lang == "en" else 1)
    return head(f"{c['name']} — {L['title_suffix']}", lang, d, desc) + mast(lang, d, f"{c['name']}/") + body


def main():
    # --check: fail if the committed pages differ from what the table renders.
    # Same discipline llms.txt and verb-reference.md already run under — the
    # site cannot say one thing while the engine does another.
    check = "--check" in sys.argv
    by_group = {}
    for c in DATA["commands"]:
        by_group.setdefault(c["group"], []).append(c["name"])
    for g in by_group:
        by_group[g].sort()

    want = {}
    for lang, L in LANGS.items():
        base = ROOT / "site" / L["dir"] / "docs" / "commands"
        want[base / "index.html"] = render_index(lang)
        for c in DATA["commands"]:
            sibs = by_group[c["group"]]
            want[base / c["name"] / "index.html"] = render_command(c, lang, sibs)

    if check:
        stale = [
            p for p, text in want.items()
            if not p.exists() or p.read_text(encoding="utf-8") != text
        ]
        if stale:
            print(f"STALE: {len(stale)} command pages differ from the verb table.")
            for p in stale[:5]:
                print(f"  {p.relative_to(ROOT)}")
            print("Regenerate with `python3 tools/gen_command_pages.py`.")
            return 1
        print(f"ok: {len(want)} command pages match the verb table")
        return 0

    for lang, L in LANGS.items():
        base = ROOT / "site" / L["dir"] / "docs" / "commands"
        if base.exists():
            shutil.rmtree(base)
    for p, text in want.items():
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")
    print(f"  wrote {len(want)} pages "
          f"({len(DATA['commands'])} commands x {len(LANGS)} languages + indexes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
