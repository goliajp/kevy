# Memory budgets — kevy-resp-client

The client owns one `TcpStream` and one reusable input/output `Vec<u8>`
per `RespClient`. Allocations are amortized across requests.

## Per-op heap allocations

| Operation                         | Allocations | Source |
|-----------------------------------|-------------|--------|
| `RespClient::connect`             | 1 (TcpStream + buffers) | `Vec::with_capacity` for the read + write buffers; `TcpStream::connect`. |
| `request(argv)`                   | 0 (warmed)  | reuses the write buffer (cleared each call) and read buffer; only the returned `Reply` allocates (per `kevy-resp`). |
| `Drop`                            | dealloc only | closes TCP socket; frees buffers. |

## Stack footprint

`size_of::<RespClient>()` = `TcpStream (8) + read_buf (24) + write_buf (24)`
≈ **56 bytes**.

## Reply allocations

`kevy_resp::Reply` allocates per the wire shape:

| Variant            | Allocations |
|--------------------|-------------|
| `Nil` / `Int(_)`   | 0           |
| `Simple`/`Error`/`Bulk` | 1 (`Vec<u8>` payload) |
| `Array(items)`     | 1 + per-item | DoS-capped at remaining-buffer bytes (`kevy-resp 0.1.0` fix). |

See [`kevy-resp/MEM-BUDGETS.md`](../kevy-resp/MEM-BUDGETS.md) for the
codec-side numbers; this crate adds only the connection wrapper.
