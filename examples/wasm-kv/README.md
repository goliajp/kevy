# kevy-embedded on WebAssembly — example

A standalone (non-workspace) example: kevy-embedded as an in-process KV cache
inside wasm, with a `wasm-bindgen` wrapper and a Node round-trip.

```sh
wasm-pack build --target nodejs
node run.cjs          # → "OK - kevy-embedded runs in wasm: core KV + del + TTL ... all work"
```

Demonstrates the **full in-memory KV including TTL** running on
`wasm32-unknown-unknown` — `set` / `get` / `del` / `set_with_ttl` / `pttl` plus
both lazy and active (reaper `tick`) expiry, verified end-to-end in Node.

`wasm32-unknown-unknown` has no `Instant`/`SystemTime`, so the host feeds time:
`run.cjs` calls `cache.set_clock(Date.now())` before TTL ops and each `tick`,
and the wrapper forwards it to kevy's `set_clock_ns` / `set_wall_clock_ms`. (An
earlier kevy trapped on every TTL op and `DEL` here, before the clock port —
see [`docs/wasm.md`](../../docs/wasm.md).)

This example lives outside the kevy workspace so kevy's own crates stay
zero-dependency — the `wasm-bindgen` dep is the app's, not kevy's.
