# kevy-vector

Approximate nearest-neighbor core for kevy: an HNSW graph over
per-shard vector sets with cosine / L2 / inner-product distances,
tombstone deletes (search-time filtered), and bounded full rebuild.
Pure logic: no I/O, no runtime, zero dependencies.
