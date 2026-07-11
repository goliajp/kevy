# kevy.golia.jp — site source

Static site for kevy: hand-written HTML/CSS, no build step, no external
requests (no CDN fonts, no image assets). The directory is
self-contained — copying `site/` to any static host is a full deploy.

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
├── CNAME               kevy.golia.jp (GitHub Pages custom domain)
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

## Deploying to GitHub Pages

Option A — Pages from a branch directory (simplest):

1. GitHub → repository **Settings → Pages**.
2. Source: **Deploy from a branch**; pick the branch and folder `/site`
   — GitHub only offers `/ (root)` and `/docs` as folders, so if `/site`
   is not selectable use Option B, or mirror `site/` into a branch root.
3. Custom domain: enter `kevy.golia.jp` (the `CNAME` file in this
   directory keeps the setting across deploys), tick **Enforce HTTPS**
   once the certificate is issued.

Option B — `gh-pages` branch with `site/` as its root:

```sh
git checkout --orphan gh-pages
git rm -rf .
git checkout <source-branch> -- site
mv site/* site/.[!.]* . 2>/dev/null; rmdir site
git add -A && git commit -m "site: deploy"
git push origin gh-pages
```

then Settings → Pages → Source: `gh-pages` branch, `/ (root)`.

DNS (at the golia.jp zone): a `CNAME` record for `kevy` pointing at
`goliajp.github.io.`. Certificate provisioning after the DNS change
takes a few minutes; **Enforce HTTPS** becomes tickable when it is done.

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
