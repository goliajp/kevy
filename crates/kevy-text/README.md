# kevy-text

Dictionary-free full-text search core for kevy: a script-aware
tokenizer (lowercased word tokens for Latin, adjacent bigrams for CJK
— no dictionary), per-shard inverted segments maintained synchronously
with writes, and BM25 ranking with shard-local statistics. Pure logic:
no I/O, no runtime.
