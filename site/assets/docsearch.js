// Docs search. No service, no library: the index is a JSON file the generator
// wrote next to this page, small enough (a few hundred entries) that scoring
// every entry per keystroke is cheaper than building anything smarter.
//
// What it has to be is FAST TO REACH: `/` focuses it from anywhere on a docs
// page, arrows move, Enter goes. A search box you have to mouse into is a
// search box people stop using.

const box = document.querySelector("#docsearch");
if (box) init();

function init() {
  const input = box.querySelector("input");
  const list = box.querySelector(".ds-list");
  const root = box.dataset.root; // "../" chain to the language root
  let entries = null;
  let sel = 0;

  let loading = null;
  function load() {
    // Idempotent, and remembered as a promise: a visitor who types faster than
    // the index fetches must not race a null `entries` into a TypeError — the
    // first version did exactly that, and the search box just went dead.
    // no-cache = revalidate, not refetch: the index JSON sits at a stable URL
    // and silently goes stale in browser caches after a docs deploy otherwise.
    loading ??= fetch(box.dataset.index, { cache: "no-cache" })
      .then((r) => r.json())
      .then((es) => (entries = es));
    return loading;
  }

  // Substring match, ranked: a hit at the start of the title beats one in the
  // middle, a command-name hit beats a section hit, shorter titles beat longer
  // (they matched more of themselves).
  function score(entry, q) {
    const t = entry.t.toLowerCase();
    const i = t.indexOf(q);
    if (i < 0) {
      const s = (entry.s || "").toLowerCase();
      const j = s.indexOf(q);
      return j < 0 ? -1 : 10 + j / 100;
    }
    const kind = entry.k === "cmd" ? 0 : entry.k === "doc" ? 1 : 2;
    return i + kind * 0.5 + t.length / 1000;
  }

  function render(q) {
    if (!q) {
      list.innerHTML = "";
      box.classList.remove("open");
      return;
    }
    if (!entries) {
      // index still in flight — re-render this same query when it lands
      load().then(() => render(q));
      return;
    }
    const hits = entries
      .map((e) => [score(e, q), e])
      .filter(([sc]) => sc >= 0)
      .sort((a, b) => a[0] - b[0])
      .slice(0, 9);
    sel = 0;
    list.innerHTML = hits
      .map(
        ([, e], i) =>
          `<a class="ds-item${i === 0 ? " sel" : ""}" href="${root}${e.u}">` +
          `<span class="ds-k ds-${e.k}">${e.k}</span>` +
          `<span class="ds-t">${esc(e.t)}</span>` +
          (e.s ? `<span class="ds-s">${esc(e.s)}</span>` : "") +
          `</a>`,
      )
      .join("");
    box.classList.toggle("open", hits.length > 0);
  }

  function esc(s) {
    return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
  }

  input.addEventListener("focus", load);
  input.addEventListener("input", () => render(input.value.trim().toLowerCase()));

  input.addEventListener("keydown", (e) => {
    const items = list.querySelectorAll(".ds-item");
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!items.length) return;
      items[sel]?.classList.remove("sel");
      sel = (sel + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length;
      items[sel]?.classList.add("sel");
      items[sel]?.scrollIntoView({ block: "nearest" });
    } else if (e.key === "Enter") {
      const target = items[sel];
      if (target) location.href = target.href;
    } else if (e.key === "Escape") {
      input.blur();
      box.classList.remove("open");
    }
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "/" && document.activeElement !== input && !e.metaKey && !e.ctrlKey) {
      // not while typing somewhere else
      const tag = document.activeElement?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;
      e.preventDefault();
      input.focus();
    }
  });

  document.addEventListener("click", (e) => {
    if (!box.contains(e.target)) box.classList.remove("open");
  });
}
