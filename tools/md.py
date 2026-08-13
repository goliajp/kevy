#!/usr/bin/env python3
"""A markdown renderer, in the house style: no dependencies.

kevy takes no third-party crates; its site takes no third-party packages either.
This handles the subset the docs actually use — which is knowable, because the
docs are in this repository and a gate checks that nothing else appears in them.

Supported: ATX headings, fenced code (with a language tag), tables, ordered and
unordered lists (one level of nesting), blockquotes, horizontal rules, and the
inline set: `code`, **bold**, *italic*, [links](…), and autolinked URLs.

Deliberately unsupported: raw HTML blocks, reference links, footnotes, setext
headings. If a doc starts needing one, add it here — do not smuggle HTML into
the markdown, because then the .md file stops being readable on GitHub, which is
where most people will actually read it.
"""

import html
import re

# ── inline ──────────────────────────────────────────────────────────────────

_CODE = re.compile(r"`([^`]+)`")
_LINK = re.compile(r"\[([^\]]+)\]\(([^)\s]+)\)")
_BOLD = re.compile(r"\*\*([^*]+)\*\*")
_ITAL = re.compile(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])")
_URL = re.compile(r"(?<![\"'=(])\bhttps?://[^\s<>)\]]+")


def inline(s, link_map=None):
    """Escape, then apply inline markup. Code spans are protected first so that
    a `*` or a `[` inside one is not eaten by the emphasis rules."""
    spans = []

    def stash(m):
        spans.append(m.group(1))
        return f"\x00{len(spans) - 1}\x00"

    s = _CODE.sub(stash, s)
    s = html.escape(s, quote=False)

    def link(m):
        text, href = m.group(1), m.group(2)
        # A backslash-escaped character in a URL is markdown's escape, not part
        # of the path: `rds\-workloads.md` is a link to rds-workloads.md, and
        # carrying the backslash through produced a 404.
        href = re.sub(r"\\(.)", r"\1", href)
        if link_map:
            href = link_map(href)
            # A map may DECLINE a link (return None) — the target is not a
            # web resource at all. Keep the text, drop the anchor: a reader
            # loses nothing, and no dead href ships.
            # `text` came out of an already-escaped string; escaping it
            # again would turn &amp; into &amp;amp;.
            if href is None:
                return text
        return f'<a href="{html.escape(href, quote=True)}">{text}</a>'

    s = _LINK.sub(link, s)
    s = _URL.sub(lambda m: f'<a href="{m.group(0)}">{m.group(0)}</a>', s)
    s = _BOLD.sub(r"<b>\1</b>", s)
    s = _ITAL.sub(r"<i>\1</i>", s)
    s = re.sub(
        r"\x00(\d+)\x00",
        lambda m: f"<code>{html.escape(spans[int(m.group(1))], quote=False)}</code>",
        s,
    )
    return s


def slug(text):
    s = re.sub(r"<[^>]+>", "", text).lower()
    s = re.sub(r"[^\w一-鿿぀-ヿ\- ]", "", s)
    return re.sub(r"[\s_]+", "-", s).strip("-") or "section"


# ── block ───────────────────────────────────────────────────────────────────


def render(md, link_map=None):
    """Return (html, toc) where toc is a list of (level, slug, text)."""
    out = []
    toc = []
    lines = md.split("\n")
    i = 0
    n = len(lines)

    while i < n:
        line = lines[i]

        # fenced code
        if line.startswith("```"):
            lang = line[3:].strip()
            body = []
            i += 1
            while i < n and not lines[i].startswith("```"):
                body.append(lines[i])
                i += 1
            i += 1
            cls = f' class="lang-{html.escape(lang, quote=True)}"' if lang else ""
            code = html.escape("\n".join(body), quote=False)
            out.append(f'<figure class="code"><pre><code{cls}>{code}</code></pre></figure>')
            continue

        # heading
        m = re.match(r"(#{1,6})\s+(.*)", line)
        if m:
            lvl = len(m.group(1))
            text = inline(m.group(2).strip(), link_map)
            sl = slug(m.group(2))
            if 2 <= lvl <= 3:
                toc.append((lvl, sl, re.sub(r"<[^>]+>", "", text)))
            out.append(f'<h{lvl} id="{sl}">{text}</h{lvl}>')
            i += 1
            continue

        # table — a header row followed by a |---|---| separator
        if line.strip().startswith("|") and i + 1 < n and re.match(r"^\s*\|[\s:|-]+\|\s*$", lines[i + 1]):
            def cells(row):
                return [c.strip() for c in row.strip().strip("|").split("|")]

            head = cells(line)
            aligns = []
            for spec in cells(lines[i + 1]):
                if spec.endswith(":") and spec.startswith(":"):
                    aligns.append(' style="text-align:center"')
                elif spec.endswith(":"):
                    aligns.append(' style="text-align:right"')
                else:
                    aligns.append("")
            i += 2
            body = []
            while i < n and lines[i].strip().startswith("|"):
                body.append(cells(lines[i]))
                i += 1
            th = "".join(
                f"<th{aligns[j] if j < len(aligns) else ''}>{inline(c, link_map)}</th>"
                for j, c in enumerate(head)
            )
            trs = "".join(
                "<tr>"
                + "".join(
                    f"<td{aligns[j] if j < len(aligns) else ''}>{inline(c, link_map)}</td>"
                    for j, c in enumerate(r)
                )
                + "</tr>"
                for r in body
            )
            out.append(f'<div class="tbl"><table><thead><tr>{th}</tr></thead><tbody>{trs}</tbody></table></div>')
            continue

        # blockquote
        if line.startswith(">"):
            body = []
            while i < n and lines[i].startswith(">"):
                body.append(lines[i].lstrip(">").strip())
                i += 1
            out.append(f'<blockquote><p>{inline(" ".join(body), link_map)}</p></blockquote>')
            continue

        # horizontal rule
        if re.match(r"^\s*(-{3,}|\*{3,}|_{3,})\s*$", line):
            out.append("<hr>")
            i += 1
            continue

        # list
        m = re.match(r"^(\s*)([-*+]|\d+\.)\s+(.*)", line)
        if m:
            ordered = m.group(2)[0].isdigit()
            tag = "ol" if ordered else "ul"
            items = []
            base = len(m.group(1))
            while i < n:
                mm = re.match(r"^(\s*)([-*+]|\d+\.)\s+(.*)", lines[i])
                if not mm or len(mm.group(1)) < base:
                    # a continuation line belongs to the item above it
                    if items and lines[i].strip() and lines[i].startswith((" ", "\t")):
                        items[-1] += " " + lines[i].strip()
                        i += 1
                        continue
                    break
                items.append(mm.group(3))
                i += 1
            lis = "".join(f"<li>{inline(t, link_map)}</li>" for t in items)
            out.append(f"<{tag}>{lis}</{tag}>")
            continue

        # blank
        if not line.strip():
            i += 1
            continue

        # paragraph
        body = []
        while i < n and lines[i].strip() and not re.match(
            r"^(#{1,6}\s|```|>|\s*([-*+]|\d+\.)\s|\s*\|)", lines[i]
        ):
            body.append(lines[i].strip())
            i += 1
        if body:
            out.append(f'<p>{inline(" ".join(body), link_map)}</p>')

    return "\n".join(out), toc
