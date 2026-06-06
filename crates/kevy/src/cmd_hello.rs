//! `HELLO` handshake reply construction — extracted from [`crate::lib`] to
//! keep that file under the 500-LOC house rule. The runtime calls
//! [`crate::KevyCommands::hello_reply`], which delegates here.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
    encode_map_header,
};

/// Parse the optional `HELLO [protover [AUTH user pass] [SETNAME name]]`
/// arguments, validate the proto switch request, and emit the right
/// reply shape (RESP2 array-of-pairs or RESP3 Map) for the resulting
/// proto. Returns `(new_proto, reply_bytes)` — the runtime applies
/// `new_proto` to the conn BEFORE folding the reply so a `HELLO 3` ack
/// itself goes out as a RESP3 Map.
///
/// Unsupported proto requests (`HELLO 4`, `HELLO 1`) leave the proto
/// unchanged + emit `-NOPROTO unsupported protocol version`. AUTH /
/// SETNAME tails are currently parsed-and-ignored (kevy has no AUTH
/// and CLIENT SETNAME is already a stub — see scope-decisions.md).
pub(crate) fn kevy_hello_reply<A: ArgvView + ?Sized>(
    args: &A,
    current_proto: RespVersion,
) -> (RespVersion, Vec<u8>) {
    let new_proto = match args.get(1) {
        None => current_proto,
        Some(b"2") => RespVersion::V2,
        Some(b"3") => RespVersion::V3,
        Some(_) => {
            let mut out = Vec::new();
            encode_error(
                &mut out,
                "NOPROTO unsupported protocol version (kevy supports 2 and 3)",
            );
            return (current_proto, out);
        }
    };
    let mut out = Vec::new();
    encode_hello_reply(&mut out, new_proto);
    (new_proto, out)
}

/// Emit the HELLO ack body shaped per `proto`. RESP2: flat
/// `*14\r\n...` array-of-pairs (kept identical to the pre-v1.4 wire
/// for backward-compat). RESP3: `%7\r\n...` Map with the same 7 fields.
fn encode_hello_reply(out: &mut Vec<u8>, proto: RespVersion) {
    let proto_int = match proto {
        RespVersion::V2 => 2,
        RespVersion::V3 => 3,
    };
    match proto {
        RespVersion::V2 => encode_array_len(out, 14),
        RespVersion::V3 => encode_map_header(out, 7),
    }
    encode_bulk(out, b"server");
    encode_bulk(out, b"kevy");
    encode_bulk(out, b"version");
    encode_bulk(out, env!("CARGO_PKG_VERSION").as_bytes());
    encode_bulk(out, b"proto");
    encode_integer(out, proto_int);
    encode_bulk(out, b"id");
    encode_integer(out, 0);
    encode_bulk(out, b"mode");
    encode_bulk(out, b"standalone");
    encode_bulk(out, b"role");
    encode_bulk(out, b"master");
    encode_bulk(out, b"modules");
    encode_array_len(out, 0);
}
