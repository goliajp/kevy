#!/usr/bin/env python3
"""The block renderer for kevy's marketing and scenario pages.

Content is data (tools/site_content/*.py); this file is the only place that
knows HTML. Three languages share one structure, so a section that exists in
English cannot quietly go missing in Japanese — the generator would raise.

Blocks: hero, prose, cards, table, code, callout, steps, split, faq.
"""

import html
import pathlib
import sys as _sys
_sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
from assetv import v as av

ROOT = pathlib.Path(__file__).resolve().parent.parent

NAV = {
    "en": {"docs": "Docs", "cmds": "Commands", "play": "Playground", "bench": "Benchmarks"},
    "zh": {"docs": "文档", "cmds": "命令", "play": "Playground", "bench": "基准"},
    "ja": {"docs": "ドキュメント", "cmds": "コマンド", "play": "Playground", "bench": "ベンチマーク"},
}
DIRS = {"en": "", "zh": "zh/", "ja": "ja/"}
HTML_LANG = {"en": "en", "zh": "zh-Hans", "ja": "ja"}


def e(s):
    return html.escape(str(s), quote=True)


def loc(href, lang_dir):
    """Put a site-internal link into the reader's own language.

    Every href in the content files is written English-relative — `docs/`,
    `play/`, `benchmarks/`. Rendered for the Chinese page they have to become
    `zh/docs/`, and they were not: the Chinese landing page's six cards and three
    buttons all pointed at the ENGLISH documentation. No link checker would ever
    have found it, because both pages exist. A reader clicking 「跑一个服务端」
    simply arrived somewhere they could not read.

    llms.txt and the like are language-neutral and stay where they are.
    """
    if not lang_dir or href.startswith(("http", "#", "llms")):
        return href
    return f"{lang_dir}{href}"


# ── blocks ──────────────────────────────────────────────────────────────────


def hero(b, up, L=""):
    ctas = "".join(
        f'<a class="cta{" primary" if i == 0 else ""}" href="{up}{loc(c["href"], L)}">{e(c["label"])}</a>'
        for i, c in enumerate(b.get("ctas", []))
    )
    if b.get("live_term"):
        t = b["live_term"]  # {"chips": [...], "hint": "..."}
        chips = "".join(
            f'<button class="ht-chip" type="button" data-cmd="{e(c)}">{e(c.split(" ")[0])}</button>'
            for c in t["chips"]
        )
        aside = f'''<div id="hero-term" data-pkg="{up}demo/pkg">
    <div class="ht-bar"><span class="ht-dot"></span><span class="ht-title">kevy — wasm</span><span class="ht-status">booting…</span></div>
    <div class="ht-out"></div>
    <div class="ht-chips">{chips}</div>
    <input class="ht-in" spellcheck="false" autocomplete="off" placeholder="{e(t["hint"])}">
  </div>'''
    else:
        aside = f'<div>{b["aside"]}</div>' if b.get("aside") else ""
    inner = f'''<div>
    <p class="eyebrow">{e(b["eyebrow"])}</p>
    <h1>{b["h1"]}</h1>
    <p class="lede">{b["lede"]}</p>
    {f'<div class="ctas">{ctas}</div>' if ctas else ""}
  </div>'''
    body = f'<div class="split">{inner}{aside}</div>' if aside else inner
    return f'<section class="band{tone(b)}"{anchor(b)} style="border-top:0">{body}</section>'


def prose(b, up, L=""):
    ps = "".join(f"<p>{p}</p>" for p in b["body"])
    h = f'<h2>{b["h2"]}</h2>' if b.get("h2") else ""
    return f'<section class="band prose{tone(b)}"{anchor(b)}>{h}{ps}</section>'


def cards(b, up, L=""):
    items = "".join(
        f"""<a class="card" href="{up}{loc(c["href"], L)}">
      <span class="card-k">{e(c["kicker"])}</span>
      <h3>{e(c["title"])}</h3>
      <p>{c["body"]}</p>
      <span class="card-go">{e(c["go"])}</span>
    </a>"""
        for c in b["items"]
    )
    h = f'<h2>{b["h2"]}</h2>' if b.get("h2") else ""   # h2 may carry <br>
    intro = f'<p class="sec-lede">{b["intro"]}</p>' if b.get("intro") else ""
    return f'<section class="band{tone(b)}"{anchor(b)}><div class="sec-h">{h}{intro}</div><div class="cards">{items}</div></section>'


def table(b, up, L=""):
    head = "".join(f"<th>{e(h)}</th>" for h in b["head"])
    rows = []
    for r in b["rows"]:
        tds = []
        for i, cell in enumerate(r):
            cls = ""
            txt = str(cell)
            # A cell the content marks with a leading ! is one where we are only
            # narrowly ahead, or behind. It gets the --loss colour on purpose:
            # a margin that thin is a fact about the reader's workload, not a
            # win to round up.
            if txt.startswith("!"):
                cls, txt = ' class="loss"', txt[1:]
            elif txt.startswith("*"):
                cls, txt = ' class="win"', txt[1:]
            elif i > 0:
                cls = ' class="num"'
            tds.append(f"<td{cls}>{txt}</td>")
        rows.append(f"<tr>{''.join(tds)}</tr>")
    h = f'<h2>{b["h2"]}</h2>' if b.get("h2") else ""
    note = f'<p class="tbl-note">{b["note"]}</p>' if b.get("note") else ""
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  <div class="sec-h">{h}{f'<p class="sec-lede">{b["intro"]}</p>' if b.get("intro") else ""}</div>
  <div class="tbl"><table><thead><tr>{head}</tr></thead><tbody>{"".join(rows)}</tbody></table></div>
  {note}
</section>"""


def code(b, up, L=""):
    cap = f'<figcaption>{b["caption"]}</figcaption>' if b.get("caption") else ""
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  {f'<div class="sec-h"><h2>{b["h2"]}</h2></div>' if b.get("h2") else ""}
  <figure class="code">{cap}<pre><code>{e(b["text"])}</code></pre></figure>
</section>"""


def callout(b, up, L=""):
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  <div class="call {b.get("kind", "note")}">
    <span class="h">{e(b["title"])}</span>
    <p>{b["body"]}</p>
  </div>
</section>"""


def steps(b, up, L=""):
    items = "".join(
        f'<li><h3>{e(s["title"])}</h3><p>{s["body"]}</p>'
        + (f'<pre><code>{e(s["code"])}</code></pre>' if s.get("code") else "")
        + "</li>"
        for s in b["items"]
    )
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  <div class="sec-h"><h2>{b["h2"]}</h2>{f'<p class="sec-lede">{b["intro"]}</p>' if b.get("intro") else ""}</div>
  <ol class="steps">{items}</ol>
</section>"""


def recipe(b, up, L=""):
    """A task recipe: the goal in one line, numbered pasteable steps with the
    expected reply inline (`->` lines, which the command gate skips), and the
    cost/limits box attached to THIS task rather than pooled at page bottom."""
    items = "".join(
        f'<li><h3>{e(s["do"])}</h3>'
        + (f'<p>{s["note"]}</p>' if s.get("note") else "")
        + f'<pre><code>{e(s["code"])}</code></pre></li>'
        for s in b["items"]
    )
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  <div class="sec-h"><h2>{b["h2"]}</h2><p class="sec-lede">{b["goal"]}</p></div>
  <ol class="steps recipe">{items}</ol>
  <div class="call loss r-cost"><span class="h">{e(b["cost_t"])}</span><p>{b["cost"]}</p></div>
</section>"""


def faq(b, up, L=""):
    items = "".join(
        f"<details><summary>{e(q['q'])}</summary><div>{q['a']}</div></details>"
        for q in b["items"]
    )
    return f'<section class="band{tone(b)}"{anchor(b)}><div class="sec-h"><h2>{b["h2"]}</h2></div><div class="faq">{items}</div></section>'


def anchor(b):
    """`id` on a block, so a hero CTA can point at the section it promises."""
    return f' id="{e(b["id"])}"' if b.get("id") else ""


def tone(b):
    """A band's value. `deep` recedes; `blue` is GOLIA's full-bleed accent and
    is spent once per page, on the thing the reader should walk away with."""
    t = b.get("tone")
    return f" {t}" if t in ("deep", "blue") else ""


def bars(b, up, L=""):
    """The measurement, at real proportions.

    Each row draws kevy's bar and the rival's from the same scale, so the chart
    cannot say something the table underneath it does not. Where the margin is
    thin the bar is short AND a different colour — a reader who only glances
    still sees which two rows are close.
    """
    peak = max(r[1] for r in b["rows"])
    rows = []
    for name, us, them, ratio, thin in b["rows"]:
        n = " narrow" if thin else ""
        rows.append(
            f'<div class="bar-row"><div class="bar-k">{e(name)}</div>'
            f'<div class="bar-track">'
            f'<div class="bar us{n}" style="width:{us / peak * 100:.1f}%"></div>'
            f'<div class="bar them" style="width:{them / peak * 100:.1f}%"></div>'
            f'</div>'
            f'<div class="bar-n{n}">{e(ratio)}</div></div>'
        )
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  <div class="sec-h">
    {f'<p class="eyebrow">{e(b["eyebrow"])}</p>' if b.get("eyebrow") else ""}
    <h2>{b["h2"]}</h2>
    {f'<p class="sec-lede">{b["intro"]}</p>' if b.get("intro") else ""}
  </div>
  <div class="bars">{"".join(rows)}</div>
  <div class="bars-legend">
    <span><i style="background:var(--blue)"></i>{e(b["us"])}</span>
    <span><i style="background:var(--edge-hi)"></i>{e(b["them"])}</span>
    <span><i style="background:var(--thin)"></i>{e(b["thin"])}</span>
  </div>
  {f'<p class="tbl-note">{b["note"]}</p>' if b.get("note") else ""}
</section>"""


def tabs(b, up, L=""):
    """Capabilities shown AS CODE. A developer skims a landing page for what
    using the thing looks like; a card that says "How ->" makes them click to
    find out, and most never do. Every command in these panels is executed
    against a real server by CI's check_site_commands gate."""
    heads = "".join(
        f'<button class="tab{" on" if i == 0 else ""}" type="button" '
        f'data-tab="{i}">{e(t["label"])}</button>'
        for i, t in enumerate(b["items"])
    )
    panels = "".join(
        f'<div class="tab-panel{" on" if i == 0 else ""}" data-panel="{i}">'
        f'<pre><code>{e(t["code"])}</code></pre>'
        + (f'<p class="tab-note">{t["note"]}</p>' if t.get("note") else "")
        + (f'<a class="tab-more" href="{up}{loc(t["href"], L)}">{e(t["go"])}</a>' if t.get("href") else "")
        + "</div>"
        for i, t in enumerate(b["items"])
    )
    return f"""<section class="band{tone(b)}"{anchor(b)}>
  <div class="sec-h">
    {f'<p class="eyebrow">{e(b["eyebrow"])}</p>' if b.get("eyebrow") else ""}
    <h2>{b["h2"]}</h2>
    {f'<p class="sec-lede">{b["intro"]}</p>' if b.get("intro") else ""}
  </div>
  <div class="tabs"><div class="tab-heads">{heads}</div>{panels}</div>
</section>"""


BLOCKS = {
    "hero": hero,
    "bars": bars,
    "tabs": tabs,
    "prose": prose,
    "cards": cards,
    "table": table,
    "code": code,
    "callout": callout,
    "steps": steps,
    "recipe": recipe,
    "faq": faq,
}


# ── page ────────────────────────────────────────────────────────────────────


def page(spec, lang, slug):
    """slug: "" for the landing page, "choose", "use/cache", …"""
    # site/use/cache/index.html is three deep; site/zh/use/cache/ is four.
    depth = (0 if lang == "en" else 1) + (slug.count("/") + 1 if slug else 0)
    up = "../" * depth
    n = NAV[lang]
    body = "\n".join(BLOCKS[b["t"]](b, up, DIRS[lang]) for b in spec["blocks"])
    # An href inside a prose body cannot know how deep its page is, and it does
    # not pass through loc(). So the content writes `~/docs/commands/` and this
    # expands it — one rule, one place, and a link that is wrong is wrong
    # everywhere rather than only on the pages nobody clicked.
    # llms.txt and llms-full.txt are language-neutral — they are generated from
    # the engine's verb table, and there is one of each. They live at the root.
    body = body.replace('href="~/llms', f'href="{up}llms')
    body = body.replace('href="~/', f'href="{up}{DIRS[lang]}')
    cur = lambda s: ' aria-current="page"' if s == slug else ""

    def lang_href(code):
        return f'{up}{DIRS[code]}{slug + "/" if slug else ""}'

    return f"""<!doctype html>
<html lang="{HTML_LANG[lang]}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{e(spec["title"])}</title>
<meta name="description" content="{e(spec["desc"])}">
<link rel="stylesheet" href="{up}assets/{av('kevy.css')}">
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
    <a class="brand" href="{up}{DIRS[lang]}">kevy<span class="v">4.0</span></a>
    <nav class="nav">
      <a href="{up}{DIRS[lang]}docs/">{n["docs"]}</a>
      <a href="{up}{DIRS[lang]}docs/commands/">{n["cmds"]}</a>
      <a href="{up}{DIRS[lang]}benchmarks/"{cur("benchmarks")}>{n["bench"]}</a>
      <a href="{up}{DIRS[lang]}play/">{n["play"]}</a>
      <a href="https://github.com/goliajp/kevy">GitHub</a>
    </nav>
    <div class="mast-right">
      <nav class="lang" aria-label="Language">
        <a href="{lang_href("en")}"{" aria-current=\"page\"" if lang == "en" else ""}>EN</a>
        <a href="{lang_href("zh")}"{" aria-current=\"page\"" if lang == "zh" else ""}>中文</a>
        <a href="{lang_href("ja")}"{" aria-current=\"page\"" if lang == "ja" else ""}>日本語</a>
      </nav>
      <button class="icon-btn" id="theme" type="button" aria-label="Toggle theme">
        <svg class="sun" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="4.2"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M19.1 4.9l-1.4 1.4M6.3 17.7l-1.4 1.4"/></svg>
        <svg class="moon" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 14.5A8.2 8.2 0 0 1 9.5 4a8.3 8.3 0 1 0 10.5 10.5z"/></svg>
      </button>
    </div>
  </div>
</header>

<main id="main">
{body}
</main>

<footer class="foot">
  <div class="foot-in">
    <p class="foot-b">kevy 4.0 — {e(spec["foot"])}</p>
    <nav>
      <a href="https://github.com/goliajp/kevy">GitHub</a>
      <a href="https://crates.io/crates/kevy">crates.io</a>
      <a href="{up}llms.txt">llms.txt</a>
    </nav>
  </div>
</footer>

{f'<script type="module" src="{up}assets/{av('hero-term.js')}"></script>' if any(b.get("live_term") for b in spec["blocks"]) else ""}
{f'<script src="{up}assets/{av('tabs.js')}" defer></script>' if any(b["t"] == "tabs" for b in spec["blocks"]) else ""}
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
