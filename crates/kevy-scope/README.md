# kevy-scope

Scoped multi-writer ownership for kevy. Per-prefix writer
declaration with optional server-backed fallback, longest-prefix
write routing, and `MOVE-SCOPE` quiesce-window migration. Pure Rust,
zero `crates.io` dependencies.

A scope is a key prefix (e.g. `app:billing:`) declared to belong to
one specific writer node. Writes for a prefix that land on the
wrong node answer `-MISDIRECTED writer is <host:port>` so a
cluster-aware client can follow.

```toml
[[cluster.scope]]
prefix   = "app:billing:"
writer   = "embed-billing-1"      # node-id of the writer
fallback = "server-eu-west-1"     # optional: takes over if writer is DOWN
```

## Migration

`MOVE-SCOPE` is operator-issued and follows a quiesce-window
protocol: the source writer quiesces, ships its prefix slice to the
new writer, then ownership flips atomically. During the window the
source returns `-QUIESCED migrating to <host:port>` so clients can
retry on the new owner.

## Out of scope

- No Raft, no gossip. The ownership table is static config; the
  `kevy-elect` quorum signals only "writer DOWN → fallback takes
  over", not topology consensus.
- No write-shadowing or double-acceptance window during migration.
- No per-key `MIGRATE` / `ASK` protocol.
- No automatic migration. `MOVE-SCOPE` is operator-issued; the
  cluster never decides to move a scope on its own.

## Audience

Internal infrastructure for the kevy server. End users configure
scopes via the server's `[cluster] scopes` TOML key and migrate them
with the `MOVE-SCOPE` operator command. See
[`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md).

## License

MIT OR Apache-2.0, at your option.
