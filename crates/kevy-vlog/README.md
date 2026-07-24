# kevy-vlog

The disposable value log under kevy's transparent tiering (the capacity
arc): cold values spill here, keys and metadata stay in RAM, and the AOF
remains the sole durability truth — the vlog is rebuilt from scratch on
every boot, so it carries **no** crash-recovery surface of its own.

- Append-only records (`[len][crc32c][key|payload]`), positional IO only
  (`read_at`/`write_at`), one `read_at` per cold read.
- Pinned readers: `Arc<VlogFile>` keeps a compacted file on disk until
  the last holder drops — snapshot views and AOF rewrites read cold
  values without racing compaction.
- Owner-driven compaction: records carry their key; the owner (the
  store's cold refs) confirms liveness and receives each survivor's new
  address. Every retirement bumps an epoch for O(1) staleness checks.

Part of the kevy workspace; pure Rust, zero crates.io dependencies.
