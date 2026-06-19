# kevy-client-async

Async client for [kevy](https://github.com/goliajp/kevy) — a
runtime-agnostic core with feature-gated transports for `tokio`,
`smol`, and `async-std`. Mirrors the blocking
[`kevy-client`](https://docs.rs/kevy-client) surface 1:1 so existing
code paths can grep-replace `Connection` → `AsyncConnection` and
add `.await`.

## Status

Phase-4 first cut (kevy v3-cluster). Surface stabilizes when the
v1.22.0 bundle ships — see the v3-cluster RFC for the locked Q4
design (mirror + pipeline-first).

## Runtime selection

This crate has **no default runtime**. Exactly one of the following
features must be enabled:

```toml
kevy-client-async = { version = "1", features = ["tokio"] }     # or "smol", or "async-std"
```

Enabling zero or more than one triggers a `compile_error!`.

## Dep-rule exemption

This is the only kevy workspace crate allowed to take a third-party
dep — the Rust async ecosystem has no std-only viable substrate. The
exemption is documented inline in `Cargo.toml` and explained in the
v3-cluster RFC (F5).
