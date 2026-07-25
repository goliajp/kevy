# kevy.golia.jp — site source

Static site for kevy: hand-written HTML/CSS, no build step, no external
requests (no CDN fonts, no image assets). The directory is
self-contained — copying `site/` to any static host is a full deploy.
The live one is `t01:/apps/kevy/web` (see Deploying).

## Layout

```
site/
├── index.html          landing (English)
├── ja/index.html       landing (Japanese)
├── zh-CN/index.html    landing (Simplified Chinese)
├── assets/site.css     shared stylesheet
├── demo/
│   ├── index.html      live wasm demo (browser REPL)
│   ├── repl.js         REPL over the @goliajp/kevy loader API
│   └── pkg/            wasm artifacts, copied from crates/kevy-wasm/pkg/
│                       (kevy.js · kevy.d.ts · kevy-opfs-worker.js · kevy.wasm)
├── CNAME               kevy.golia.jp (legacy; domain is configured on the box)
└── README.md           this file
```

Refreshing the demo after a wasm rebuild:

```sh
cargo build -p kevy-wasm --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/kevy_wasm.wasm crates/kevy-wasm/pkg/kevy.wasm
cp crates/kevy-wasm/pkg/{kevy.js,kevy.d.ts,kevy-opfs-worker.js,kevy.wasm} site/demo/pkg/
```

## Local preview

```sh
python3 -m http.server --directory site <port>
```

Any static file server works; the demo page needs `http://` (not
`file://`) because it uses ES modules and a worker.

## Deploying

The site is served from **`t01:/apps/kevy/web`**. DevOps owns the box,
the web server and TLS for `kevy.golia.jp`; that groundwork is already
done and is not ours to re-create. Deploying is therefore just
**updating the files in place** — there is no build step and nothing to
compile.

```sh
# From the repo root, after the wasm bundle in site/demo/pkg/ is current.
rsync -av --delete site/ t01:/apps/kevy/web/
```

`--delete` keeps the target an exact mirror, so a file removed here
disappears there. Check `site/demo/pkg/` first: the demo loads
`kevy.wasm` / `kevy.js` from it, and those are build artifacts that must
be refreshed from `crates/kevy-wasm/pkg/` whenever the wasm changes (see
the wasm bump step in the release checklist).

The `CNAME` file is a leftover of an earlier GitHub Pages plan. It is
harmless on a plain static host and is kept only so the directory stays
portable; the live domain is configured on the box, not by this file.

## Header requirements: none

The demo needs no COOP/COEP (cross-origin isolation) headers — GitHub
Pages cannot set custom headers, and that is fine:

- The OPFS backend uses a `FileSystemSyncAccessHandle` inside a
  dedicated worker, communicating over plain `postMessage`. No
  `SharedArrayBuffer`, no `Atomics.wait`, so no isolation requirement
  (verified against `demo/pkg/kevy-opfs-worker.js` — async messaging
  only).
- Where OPFS sync handles are unavailable the loader falls back to
  IndexedDB automatically, which likewise needs no isolation.
- Cross-tab pub/sub uses `BroadcastChannel`, same-origin only, no
  headers involved.
