// Renderer code — a normal web page script. It never touches the engine or
// Node; everything goes through the async `window.kevy` the preload exposed.
// Safe under contextIsolation + sandbox.

const $ = (id) => document.getElementById(id);

// ── key / value ──────────────────────────────────────────────────────────
$("set").onclick = async () => {
  await window.kevy.set($("key").value, $("val").value);
  $("kvOut").textContent = `SET ${$("key").value} → OK`;
};

$("get").onclick = async () => {
  const v = await window.kevy.getText($("key").value);
  $("kvOut").textContent = v === undefined ? `GET ${$("key").value} → (nil)` : `GET ${$("key").value} → ${v}`;
};

// ── live pub/sub ─────────────────────────────────────────────────────────
const log = (line) => {
  const el = document.createElement("div");
  el.textContent = `${new Date().toLocaleTimeString()}  ${line}`;
  $("log").prepend(el);
};

// Subscribe once at startup; every PUBLISH below round-trips through the
// engine in the main process and streams back here over IPC.
window.kevy.subscribe($("chan").value, (payload, channel) => {
  log(`▼ ${channel}: ${new TextDecoder().decode(payload)}`);
});

$("pub").onclick = async () => {
  const n = await window.kevy.publish($("chan").value, $("msg").value);
  log(`▲ published to ${$("chan").value} (${n} subscriber${n === 1 ? "" : "s"})`);
};

window.kevy.version().then((v) => log(`kevy engine ${v} ready`));
