# kevy-scope

**Scoped multi-writer ownership table for kevy** — per-prefix writer
declaration with optional server-backed fallback, longest-prefix
write routing, and `MOVE-SCOPE` quiesce-window migration. Pure Rust,
zero `crates.io` dependencies.

Implements Phase 3 of the v3-cluster RFC. A scope is a key prefix
(e.g. `app:billing:`) declared to belong to one specific writer node
(an embed-as-writer process, the v1.21+ topology). Writes for a
prefix that land on the *wrong* node answer `-MISDIRECTED writer is
<host:port>` so the kevy-cluster-rw client can follow.

```toml
[[cluster.scope]]
prefix   = "app:billing:"
writer   = "embed-billing-1"      # node-id of the writer
fallback = "server-eu-west-1"     # optional: takes over if writer is DOWN
```

## Anti-scope (locked in the v3-cluster RFC)

- **No Raft, no gossip.** The ownership table is static config; the
  elect quorum (`kevy-elect`) signals only "writer DOWN → fallback
  takes over", not topology consensus.
- **No write-shadowing during migration.** The Q3 resolution locks
  `MOVE-SCOPE` to **option (a) quiesce-window**: the writer
  quiesces, ships its prefix slice, then ownership flips. No
  double-acceptance window, no per-key MIGRATE/ASK protocol.
- **No automatic migration.** `MOVE-SCOPE` is operator-issued; the
  cluster never decides to move a scope on its own.

See `.claude/rfcs/2026-06-18-v3-cluster.md` `## Q3 resolution` for
the long-form rationale.

## Status

Phase 3 in progress on `feature/v3-3-scope`. Not yet released.
