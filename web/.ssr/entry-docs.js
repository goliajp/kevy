import { jsxs, jsx, Fragment } from "react/jsx-runtime";
import { renderToStaticMarkup } from "react-dom/server";
import { Package, ArrowUpRight } from "lucide-react";
const LICENSE = {
  en: (y) => `MIT or Apache-2.0 · © ${y} GOLIA K.K.`,
  zh: (y) => `MIT 或 Apache-2.0 · © ${y} GOLIA K.K.`,
  ja: (y) => `MIT または Apache-2.0 · © ${y} GOLIA K.K.`
};
const LINKS = [
  { label: "GitHub", href: "https://github.com/goliajp/kevy" },
  { label: "crates.io", href: "https://crates.io/crates/kevy" },
  { label: "docs.rs", href: "https://docs.rs/kevy" }
];
function Footer({ lang }) {
  return /* @__PURE__ */ jsxs("footer", { children: [
    /* @__PURE__ */ jsxs("div", { children: [
      /* @__PURE__ */ jsx("a", { className: "org", href: "https://golia.jp", target: "_blank", rel: "noreferrer", children: "GOLIA" }),
      /* @__PURE__ */ jsx("div", { children: LICENSE[lang]((/* @__PURE__ */ new Date()).getFullYear()) })
    ] }),
    /* @__PURE__ */ jsx("div", { className: "links", children: LINKS.map(({ label, href }) => /* @__PURE__ */ jsxs("a", { href, target: "_blank", rel: "noreferrer", children: [
      /* @__PURE__ */ jsx(Package, { size: 13, strokeWidth: 2 }),
      label,
      /* @__PURE__ */ jsx(ArrowUpRight, { size: 12, strokeWidth: 2, className: "ext" })
    ] }, label)) })
  ] });
}
const LANG_LABEL = { en: "EN", zh: "中文", ja: "日本語" };
const LANG_HTML = { en: "en", zh: "zh-CN", ja: "ja" };
const DOCS_LABEL = { en: "Docs", zh: "文档", ja: "ドキュメント" };
const HOME_LABEL = { en: "Home", zh: "首页", ja: "ホーム" };
const ON_THIS_PAGE = {
  en: "On this page",
  zh: "本页内容",
  ja: "このページ"
};
function up(depth) {
  return depth === 0 ? "./" : "../".repeat(depth);
}
function twin(lang, slug, depth) {
  const root = up(depth);
  return lang === "en" ? `${root}docs/${slug}/` : `${root}${lang}/docs/${slug}/`;
}
function Doc(p) {
  const root = up(p.depth);
  return /* @__PURE__ */ jsxs(Fragment, { children: [
    /* @__PURE__ */ jsx("header", { className: "masthead", children: /* @__PURE__ */ jsxs("div", { className: "masthead-inner", children: [
      /* @__PURE__ */ jsxs("a", { className: "brand", href: p.lang === "en" ? root : `${root}${p.lang}/`, children: [
        /* @__PURE__ */ jsx("span", { className: "wordmark", children: "kevy" }),
        /* @__PURE__ */ jsx("span", { className: "ver", children: p.version })
      ] }),
      /* @__PURE__ */ jsxs("nav", { className: "topnav", children: [
        /* @__PURE__ */ jsx("a", { href: p.lang === "en" ? root : `${root}${p.lang}/`, children: HOME_LABEL[p.lang] }),
        /* @__PURE__ */ jsx("a", { href: p.lang === "en" ? `${root}docs/` : `${root}${p.lang}/docs/`, children: DOCS_LABEL[p.lang] }),
        /* @__PURE__ */ jsx("div", { className: "langswitch", role: "group", "aria-label": "language", children: ["en", "zh", "ja"].map(
          (l) => (
            // A language the page does not exist in is not offered. An
            // offer that 404s is worse than no offer.
            p.have.includes(l) ? /* @__PURE__ */ jsx(
              "a",
              {
                className: l === p.lang ? "on" : "",
                href: twin(l, p.slug, p.depth),
                hrefLang: LANG_HTML[l],
                children: LANG_LABEL[l]
              },
              l
            ) : null
          )
        ) })
      ] })
    ] }) }),
    /* @__PURE__ */ jsxs("div", { className: "docshell", children: [
      /* @__PURE__ */ jsx("nav", { className: "docnav", "aria-label": DOCS_LABEL[p.lang], children: p.nav.map((g) => /* @__PURE__ */ jsxs("div", { children: [
        /* @__PURE__ */ jsx("div", { className: "group", children: g.label }),
        g.items.map((it) => /* @__PURE__ */ jsx(
          "a",
          {
            className: it.slug === p.slug ? "on" : "",
            href: twin(p.lang, it.slug, p.depth),
            "aria-current": it.slug === p.slug ? "page" : void 0,
            children: it.title
          },
          it.slug
        ))
      ] }, g.id)) }),
      /* @__PURE__ */ jsxs("main", { className: "docmain", children: [
        /* @__PURE__ */ jsxs("div", { className: "breadcrumb", children: [
          /* @__PURE__ */ jsx("a", { href: p.lang === "en" ? root : `${root}${p.lang}/`, children: "kevy" }),
          " / ",
          /* @__PURE__ */ jsx("a", { href: p.lang === "en" ? `${root}docs/` : `${root}${p.lang}/docs/`, children: DOCS_LABEL[p.lang] })
        ] }),
        p.toc.length > 2 && /* @__PURE__ */ jsxs("details", { className: "toc", children: [
          /* @__PURE__ */ jsx("summary", { children: ON_THIS_PAGE[p.lang] }),
          /* @__PURE__ */ jsx("ul", { children: p.toc.map((t) => /* @__PURE__ */ jsx("li", { className: `l${t.level}`, children: /* @__PURE__ */ jsx("a", { href: `#${t.slug}`, children: t.text }) }, t.slug)) })
        ] }),
        /* @__PURE__ */ jsx("div", { dangerouslySetInnerHTML: { __html: p.bodyHtml } })
      ] })
    ] }),
    /* @__PURE__ */ jsx("div", { className: "shell", children: /* @__PURE__ */ jsx(Footer, { lang: p.lang }) })
  ] });
}
function renderDocPage(p, cssHref) {
  const body = renderToStaticMarkup(/* @__PURE__ */ jsx(Doc, { ...p }));
  const alt = p.have.filter((l) => l !== p.lang).map(
    (l) => `<link rel="alternate" hreflang="${LANG_HTML[l]}" href="https://kevy.golia.jp${l === "en" ? "/docs/" : `/${l}/docs/`}${p.slug}/">`
  ).join("\n    ");
  return `<!doctype html>
<html lang="${LANG_HTML[p.lang]}">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>${escapeAttr(p.title)} · kevy</title>
    <meta name="description" content="${escapeAttr(p.desc)}">
    <link rel="canonical" href="https://kevy.golia.jp${p.lang === "en" ? "/docs/" : `/${p.lang}/docs/`}${p.slug}/">
    ${alt}
    <meta name="color-scheme" content="light">
    <meta name="theme-color" content="#fcfbf8">
    <link rel="icon" href="${up(p.depth)}kevy-logo.svg" type="image/svg+xml">
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap" rel="stylesheet">
    <link rel="stylesheet" href="${up(p.depth)}${cssHref}">
  </head>
  <body>
${body}
  </body>
</html>
`;
}
function escapeAttr(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
export {
  renderDocPage
};
