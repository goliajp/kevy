// Capability tabs. Plain buttons and panels — no state beyond a class, nothing
// to hydrate, and the first panel is visible before any script runs, so a
// visitor with JavaScript disabled still sees code instead of an empty box.
for (const w of document.querySelectorAll(".tabs")) {
  const heads = w.querySelectorAll(".tab");
  const panels = w.querySelectorAll(".tab-panel");
  for (const h of heads) {
    h.addEventListener("click", () => {
      for (const x of heads) x.classList.remove("on");
      for (const x of panels) x.classList.remove("on");
      h.classList.add("on");
      w.querySelector(`[data-panel="${h.dataset.tab}"]`).classList.add("on");
    });
  }
}
