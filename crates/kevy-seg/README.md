# kevy-seg

Immutable ordered-record segment files — build once, binary-search forever.

The storage primitive every cold tier shares. Row segments, scalar-index cold
segments and text bucket segments differ only in what their records *mean*;
how records are laid out, located, checksummed and retired is identical, so it
lives here once instead of three times.

## Model

`SegBuilder` appends records in strictly ascending key order, pages them,
checksums every page, and seals the file with a footer: record count, min/max
key, and a fence table holding the first key of every page.

`Seg` opens a sealed file, keeps the fence table in memory, and answers
`get` / `range` / `count_range` with **one fence binary search plus one page
read**.

Nothing ever mutates a sealed segment. Deletion is the caller's directory
concern — tombstones live above this crate — and removal is `unlink`.

## Layout (v1)

Data pages are 4 KiB: a small header, cells packed forward, a slot directory
(`u16` cell offsets) packed backward from the tail, and a CRC32C over the page
in the last four bytes. A record too large for one page spills its payload into
a run of dedicated overflow pages — SQLite's overflow idea, shorn of its
freelist because nothing here is ever freed. The footer is written last (fence
entries, min/max keys, counts, magic, its own CRC) and the final 16 bytes
locate and size it.
The format is documented in full in the crate docs, so a reader can implement
it from the source rather than from a specification kept somewhere else.

Pure Rust, zero dependencies. Part of [kevy](https://github.com/goliajp/kevy).

Licensed under Apache-2.0 OR MIT.
