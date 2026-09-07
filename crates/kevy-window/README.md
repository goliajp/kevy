# kevy-window

The sliding-window runtime for scalar and text indexes.

Shared by the kevy server and the embedded store, so the two faces cannot
drift: boundary maintenance, the eviction slide, and the cold half of
`range` / `count` are one implementation, not two.

## Why a slide can fail safely

Cold segments are **derived spill, not truth** — the rows stay hot and the
index is rebuilt from them on boot. So a failed slide simply leaves the tree
untouched: the batch is read before it is cut, and a restart drops the segment
set and re-slides from the rows.

That property is the reason this crate can be aggressive about eviction
without needing a crash-recovery protocol of its own.

Every public item is documented, and the lint holds it that way.

Pure Rust, zero dependencies. Part of [kevy](https://github.com/goliajp/kevy).

Licensed under Apache-2.0 OR MIT.
