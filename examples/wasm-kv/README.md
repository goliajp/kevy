# kevy-embedded on WebAssembly — example

A standalone (non-workspace) example: kevy-embedded as an in-process KV cache
inside wasm, with a `wasm-bindgen` wrapper and a Node round-trip.

```sh
wasm-pack build --target nodejs
node run.cjs          # → "OK - kevy-embedded core KV (set/get/dbsize) runs in wasm in-memory."
```

Demonstrates the **core in-memory KV** (`set` / `get` / `dbsize`) running on
`wasm32-unknown-unknown` — verified, ~139 KB wasm.

⚠️ TTL/expiry + `DEL` are intentionally **not** wrapped: they panic on
`wasm32-unknown-unknown` today (the clock reads `Instant::now()`, unimplemented
there). See [`docs/wasm.md`](../../docs/wasm.md) and the roadmap clock-port item.
This example lives outside the kevy workspace so kevy's own crates stay
zero-dependency — the `wasm-bindgen` dep is the app's, not kevy's.
