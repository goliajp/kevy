# kevy-mcp

kevy's official [MCP](https://modelcontextprotocol.io) server: a stdio
JSON-RPC 2.0 bridge (protocol revision 2024-11-05) that exposes a running
kevy server to AI agents as five tools. Pure Rust, `std` + internal
`kevy-*` crates only.

```
kevy-mcp [--url redis://127.0.0.1:6004] [--allow-writes]
```

On startup it connects to kevy and bootstraps the verb catalog from
`COMMAND DOCS` — the single source of truth shared with the engine's
dispatch table — so the read/write whitelists can never drift from the
server it talks to.

## Tools

| tool | params | notes |
|---|---|---|
| `kevy_discover` | `verb?` | live verb documentation table (COMMAND DOCS) |
| `kevy_read` | `command: string[]` | read-only verbs only; write verbs rejected |
| `kevy_write` | `command: string[]` | opt-in: hidden + rejected without `--allow-writes` |
| `kevy_explain` | `index, args?: string[]` | `IDX.EXPLAIN` pass-through (query plans) |
| `kevy_info` | `section?` | `INFO` pass-through |

Server `-ERR …` replies come back as tool results with `isError: true`
and the original error text preserved verbatim. The process exits
cleanly on stdin EOF.

## Claude Code registration

```
claude mcp add kevy -- kevy-mcp --url redis://127.0.0.1:6004
```
