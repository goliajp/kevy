# `kevy-replicate` — snapshot ship protocol

Status: Phase 1.E (v3-cluster). Wire-layer protocol locked for v1.18.0;
reactor + store wiring lands at T1.23/T1.24/T1.25/T1.26.

## When the primary sends a snapshot

The primary's [`crate::source::ReplicationSource`] holds a bounded
backlog (default 256 MiB). A replica connects via
[`crate::replica::ReplicaClient`] with `REPLICATE FROM <from_offset>`;
the primary chooses one of two flows based on whether the backlog can
serve from `from_offset`:

- **Resume path** (the happy case): `source.frames_from(from_offset)`
  returns `Ok(iter)`. Primary replies `+ACK <next_offset>\r\n` and
  starts streaming wire-frames (`*2\r\n:<offset>\r\n<argv>` — see
  `wire.md`). The replica's `ReplicaClient` decodes each via
  `decode_frame` and applies through the user's dispatcher.
- **Snapshot path**: `source.frames_from(from_offset)` returns
  `Err(TooOld)` (replica missed the backlog window) OR `from_offset == 0`
  with a non-empty store (fresh replica needs the keyspace). Primary
  replies `+ACK <ack_offset>\r\n` (still — the ack offset on this
  path equals the snapshot's "as-of" offset, see [Offset semantics]
  below), then a snapshot stream, then live frames.

The primary picks the path before sending anything beyond `+ACK`, so
the replica only sees one of the two byte sequences after the ack
line and can branch deterministically on the first byte of the next
read.

## Wire shape

A snapshot ship is sandwiched between two control lines:

```
+SNAPSHOT\r\n                    <- snapshot begin marker
$L1\r\n<L1 bytes>\r\n            <- chunk 1: RESP bulk string
$L2\r\n<L2 bytes>\r\n            <- chunk 2
...
$LN\r\n<LN bytes>\r\n            <- chunk N
+SNAPSHOT_END <ack_offset>\r\n   <- snapshot end marker; ack_offset is
                                    the source's next_offset at the
                                    moment the snapshot started, so the
                                    replica resumes live streaming from
                                    `ack_offset` (next frame the primary
                                    will send has offset == ack_offset).
```

Every byte the snapshot serializer produces is concatenated into
RESP bulk-string chunks of at most [`SNAPSHOT_CHUNK_MAX`] bytes each
(default 64 KiB). The primary picks chunk boundaries arbitrarily —
the snapshot's logical structure (kevy-persist RDB-style records) is
the replica's problem to deserialize, not the wire's. Chunks may
have any length from `1` to `SNAPSHOT_CHUNK_MAX`; the replica
appends each into a single growing buffer and hands it to
`kevy_persist::load_snapshot` once `+SNAPSHOT_END` arrives.

### Why RESP bulk strings inside markers

The framing reuses RESP primitives so any RESP-aware debug capture
(`nc primary 16004 | redis-cli --pipe-mode`) parses a snapshot stream
without a custom tool. The `+SNAPSHOT` / `+SNAPSHOT_END` markers are
RESP simple strings (standard); chunks are RESP bulk strings
(standard). The "logical envelope" that says "the bulk strings
between these two markers are one snapshot" is implied by the
markers themselves, not encoded as a length prefix — so the primary
doesn't need to know the snapshot's total size before starting (no
buffer-and-back-patch).

## Offset semantics

`ack_offset` carries through the whole exchange so the replica never
has to compute it:

- After `+ACK <ack_offset>\r\n`, the replica records `ack_offset` as
  `primary_offset_at_handshake` (see `ReplicaClient`).
- If primary takes the **resume path**, the next frame's offset is
  `ack_offset`. The replica's `expected_offset` was initialised to
  `from_offset` at connect; it doesn't auto-advance to `ack_offset` —
  instead, a gap shows up as `OffsetGap { expected: from_offset, got:
  ack_offset }` for the very first frame. This is the **snapshot-
  resync signal**: the replica catches the gap, drops its local
  store, then expects the primary to switch to the snapshot path on
  the next reconnect. **v1.18.0 in-process recipe**: the caller sees
  `OffsetGap`, reconnects, and primary's snapshot-path code path
  fires. **v1.19.0+** will let the primary decide before the gap and
  send `+SNAPSHOT` immediately (no client-driven reconnect needed).
- If primary takes the **snapshot path**, the snapshot's "as-of"
  offset == `ack_offset`. After `+SNAPSHOT_END <ack_offset>` lands,
  the replica's `expected_offset` jumps to `ack_offset` and the next
  live frame arrives at `ack_offset` (so there is no offset gap).

The two paths converge: after either flow, `expected_offset ==
primary.next_offset()` and live streaming proceeds normally.

## Caps + defaults

| constant | default | rationale |
|---|---|---|
| `SNAPSHOT_CHUNK_MAX` | 64 KiB | matches a typical TCP segment + a debug-friendly stride |
| `SNAPSHOT_TOTAL_CAP` | 16 GiB | hard ceiling for the replica's accumulating buffer; over → drop link |
| `SNAPSHOT_LINE_MAX` | 256 B | cap on a single control line (`+SNAPSHOT_END ...`) |

The replica drops the connection if a chunk's `$L` length exceeds
`SNAPSHOT_CHUNK_MAX`, the running total exceeds `SNAPSHOT_TOTAL_CAP`,
or a control line exceeds `SNAPSHOT_LINE_MAX`. All three are
defensive against a misbehaving / hostile primary; a real primary
respects all three.

## Interaction with live frames

**v1.18.0 simplification**: during the snapshot ship, the primary
sends *only* snapshot bytes between `+SNAPSHOT` and `+SNAPSHOT_END`.
No live `*2\r\n` frames are interleaved. The replica's parser is
free to assume "one segment at a time".

T1.25 will revisit this — the primary may need to interleave live
frames with the snapshot so a slow snapshot doesn't lag fresh writes
past the backlog window. The wire format already allows this (a
chunk's bulk-string framing is unambiguous; control lines parse
distinctly), but v1.18 ships the simpler "no interleaving" semantics
and the replica's state machine refuses any non-chunk bytes between
`+SNAPSHOT` and `+SNAPSHOT_END`.

## Encoders + decoders

This module exposes wire-layer helpers (see `crate::wire`):

- `encode_snapshot_begin() -> Vec<u8>` — `+SNAPSHOT\r\n`
- `encode_snapshot_chunk(bytes: &[u8]) -> Vec<u8>` — `$L\r\n<bytes>\r\n`
- `encode_snapshot_end(ack_offset: u64) -> Vec<u8>` — `+SNAPSHOT_END <off>\r\n`
- `decode_snapshot_marker(&[u8]) -> Result<Option<(SnapshotMarker, usize)>, WireError>`
  — peek the next line to detect `+SNAPSHOT` / `+SNAPSHOT_END <off>`.
- The chunk decoder reuses [`crate::wire::parse_bulk_chunk`]
  (RESP bulk string parse, same path AOF entries use elsewhere in
  the workspace).

`ReplicaClient` exposes the snapshot-aware streaming surface via a
new event-returning iterator; see `crate::replica::ReplicaClient::next_event`.

## Extension points (deferred — not in v1.18.0)

- **Snapshot resumption**: replica was 90% done downloading, peer
  dropped. Today it restarts from scratch. A "snapshot session id" +
  offset on the begin marker would let it resume.
- **Snapshot compression**: gzip / zstd around the chunk bytes. Same
  envelope, new chunk type byte. Measure before adding.
- **Interleaved live frames during snapshot**: T1.25; needs a per-
  frame type tag the replica parser can fork on.
