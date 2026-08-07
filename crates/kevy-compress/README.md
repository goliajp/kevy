# kevy-compress

Corpus-aware LZ compression for tiered value logs: a dictionary-assisted
fast level with never-expanding frames and wildcopy-class decode.

Built for the shape kevy's value log has: records from one keyspace land
together, so their redundancy lives *across* values — field names, enum
members, shared prefixes — where a per-datum compressor cannot see it. A
dictionary trained on the file (and seeded across rotation) reaches it.

- `encode(dict, input) -> Vec<u8>` — never longer than input + 6-byte
  header; incompressible input is stored raw by construction.
- `decode(dict, frame) -> Result<Vec<u8>, Corrupt>` — bounds-checked,
  `forbid(unsafe_code)`; corrupt or truncated frames are rejected, never
  mis-decoded.
- `train(samples, budget) -> Vec<u8>` — dictionary construction as a
  replaceable policy; the dictionary is a parameter, never a dependency.

`no_std` + `alloc`, zero dependencies.
