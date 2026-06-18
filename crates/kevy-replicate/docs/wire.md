# `kevy-replicate` — wire format

Status: Phase 1 (v3-cluster). Locked for v1.18.0.

## Goals

- **Re-use what the server already speaks.** Every replicated mutation is
  a Redis command the primary just applied. The replica re-applies it via
  the same parser / dispatcher path that AOF replay uses. The wire format
  is therefore "RESP2 multi-bulk command + a small offset envelope" —
  not a new serialization.
- **Stream-shaped.** Frames are length-self-describing; concatenation
  is the protocol. No outer length prefix the sender has to back-patch.
- **Forward by offset.** Every frame carries a monotonic, primary-
  assigned `u64` offset. Replicas ACK by offset; reconnecting replicas
  ask `REPLICATE FROM <offset>` and the primary replays from its ring
  buffer (or triggers a full snapshot if `<offset>` has been evicted).
- **No checksum, for now.** TCP already carries a 16-bit checksum;
  payload corruption inside a single TCP stream is exceedingly rare on
  the LAN this protocol targets. A checksum lane can be added as an
  envelope extension (see "Extension points") if benchmarks ever show
  it's needed.

## Frame layout

```
*2\r\n                       <- envelope: outer multi-bulk with exactly 2 elements
:<offset>\r\n                <- element 1: RESP integer, the monotonic u64 offset
*N\r\n                       <- element 2: RESP2 multi-bulk command, the inner argv
$L1\r\n<arg1>\r\n
$L2\r\n<arg2>\r\n
...
$LN\r\n<argN>\r\n
```

The envelope is itself a RESP2 array of 2 elements — chosen so a debug
client (`nc primary 16004 | tee stream.bin`) can be replayed through
any RESP2-aware tool without a custom parser. The inner `*N\r\n…`
payload is byte-for-byte identical to what the original client sent
when issuing the command, so feeding it back through `kevy_resp::
parse_command_into` reconstructs the same `Argv` the primary applied.

### `offset`

- `u64` in the Rust API, but the wire envelope uses a RESP integer
  (signed by spec), so **the wire constraint is `offset ≤ i64::MAX`**
  (≈ 9.2 EB of frames; ~30,000 years at 10 M writes/s). Real
  deployments are not at risk. `encode_frame` `debug_assert!`s the
  bound; a release build that ignores it produces a frame the peer
  rejects with `BadOffset`.
- Monotonically increasing within a single primary's lifetime.
- Assigned at apply time (after the mutation lands in the local store),
  not at command receipt — so offsets order the commit log, not the
  request stream.
- Encoded as a RESP integer (`:N\r\n`). Negative integers reject with
  `WireError::NegativeOffset` since offsets are unsigned.
- A primary that loses its persisted offset state (cold disk wipe)
  starts a new offset epoch (`epoch` field of `+ACK`, future work) so
  replicas detect the discontinuity and full-sync. v1.18.0 ships
  without epoch tracking; operators rebuild replicas manually when the
  primary's data dir is wiped. Tracked in plan T1.20.

### Inner payload

A RESP2 multi-bulk frame: `*N\r\n` followed by N `$L\r\n<bytes>\r\n`
bulk strings. No inline-form support (replication is server-to-server;
inline form is a debug convenience only the client side needs).

Allowed commands: every command that *mutates* state and is replication-
safe (i.e. deterministic given the local store + this argv). The
allowlist is enforced at apply time by the dispatcher, not at decode
time — the wire format itself is command-agnostic.

## Reading multiple frames from a buffer

Decoding is **incremental**:

```rust
let mut pos = 0;
loop {
    match decode_frame(&buf[pos..]) {
        Ok((offset, argv, used)) => {
            apply(offset, argv);
            pos += used;
        }
        Err(WireError::Truncated) => break, // read more bytes, retry
        Err(other) => return Err(other),    // hard error; tear down link
    }
}
buf.drain(..pos);
```

`used` is the number of bytes the decoded frame consumed, exactly as
`parse_command_into` returns `consumed`. Callers manage their own
read buffer; the wire module is allocation-free except for the per-
frame `Argv` it returns.

## Encoding

```rust
fn encode_frame(offset: u64, argv: &Argv) -> Vec<u8>
```

Returns a freshly allocated `Vec<u8>` of exactly the bytes shown
above. Hot-path callers should prefer a future `encode_frame_into(&mut
Vec<u8>, offset, &Argv)` variant (deferred until benchmarks justify
the second entry point — Phase 1 source-side code path builds the
vector once per outgoing frame and is not allocation-sensitive).

## Errors

```rust
pub enum WireError {
    Truncated,                            // need more bytes, retry later
    BadEnvelope,                          // not `*2\r\n` outer header
    BadOffset,                            // offset element not a RESP integer
    NegativeOffset(i64),                  // RESP integer is negative
    BadPayload(kevy_resp::ProtocolError), // inner command malformed
}
```

`Truncated` is the only soft error — callers should accumulate more
bytes and call `decode_frame` again. The other variants signal a
corrupt or protocol-violating peer; the right response is to drop the
connection and let the replica re-handshake (which may trigger a full
snapshot).

## Extension points (deferred — not implemented in v1.18.0)

- **Envelope to `*3` with a checksum element.** Add a third element
  `$8\r\n<8 bytes>\r\n` carrying a u64 hash over the inner payload.
  Old replicas reading new frames would see `*3` and refuse with
  `BadEnvelope`; the upgrade path is a feature flag on the handshake.
- **Per-frame timestamp.** Useful for observability; same envelope
  expansion approach.
- **Multi-frame batching.** A `*1\r\n<*N…>` wrapper would let the
  primary coalesce N small mutations into one syscall — measure under
  load first.

All extensions follow the same rule: keep the inner command payload
byte-identical to a client-issued RESP2 request so the apply path
stays one code path.
