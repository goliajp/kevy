//! `CLIENT *` subcommands. v1.0 ships canonical stub replies that
//! every Redis client library accepts at handshake / housekeeping
//! time, without needing per-connection state plumbed through the
//! reactor → dispatch boundary.
//!
//! Real per-connection tracking (CLIENT LIST showing N connections,
//! CLIENT KILL actually closing them, CLIENT SETNAME persisting the
//! name on the conn) needs a `&mut Conn` argument added to the
//! `kevy_rt::Commands::dispatch_into` trait signature — a bigger
//! change deferred to v1.x. For dev/docker-compose/embedded/cache
//! scenarios these stubs cover the 95% case where a client calls
//! CLIENT once at handshake and never inspects.

use kevy_resp::{
    ArgvView, RespVersion, encode_bulk, encode_error, encode_integer, encode_simple_string,
    encode_verbatim,
};

use super::wrong_args;

pub(crate) fn cmd_client<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "client"),
    };
    match sub.as_slice() {
        // ID: 1 unique-but-stable for the lifetime of the process. Clients
        // that need uniqueness across reconnects don't get it; we don't
        // track per-conn ids in v1.0.
        b"ID" => encode_integer(out, 1),
        // GETNAME: empty bulk (no name set — we don't track per-conn names).
        b"GETNAME" => encode_bulk(out, &[]),
        // SETNAME <name>: accept and discard. Redis clients use this
        // for human-readable identification in CLIENT LIST output;
        // since our CLIENT LIST is also a stub, the discard is consistent.
        b"SETNAME" => {
            if args.len() != 3 {
                wrong_args(out, "client|setname");
            } else {
                encode_simple_string(out, "OK");
            }
        }
        // LIST: single canonical entry representing this connection.
        // Field set documented at https://redis.io/commands/client-list/.
        // Most fields are zero / placeholder; clients that PARSE this
        // field-by-field (e.g. for monitoring) will need real per-conn
        // state in v1.x.
        b"LIST" => {
            let body = "id=1 addr=127.0.0.1:0 laddr=127.0.0.1:0 fd=0 name= \
                        age=0 idle=0 flags=N db=0 sub=0 psub=0 ssub=0 \
                        multi=-1 watch=0 qbuf=0 qbuf-free=0 argv-mem=0 \
                        multi-mem=0 tot-mem=0 rbs=0 rbp=0 obl=0 oll=0 omem=0 \
                        events=r cmd=client|list user=default redir=-1 \
                        resp=2 lib-name= lib-ver=\n";
            emit_client_text(out, body.as_bytes(), proto);
        }
        // KILL: zero connections actually killed (stub).
        b"KILL" => encode_integer(out, 0),
        // INFO: single-line info about THIS connection.
        b"INFO" => {
            let body = "id=1 addr=127.0.0.1:0 laddr=127.0.0.1:0 fd=0 name= \
                        age=0 idle=0 flags=N db=0 sub=0 psub=0 ssub=0 \
                        multi=-1 cmd=client|info user=default resp=2";
            emit_client_text(out, body.as_bytes(), proto);
        }
        // NO-EVICT: accept; v1.0 has no eviction so this is trivially honored.
        b"NO-EVICT" => encode_simple_string(out, "OK"),
        // PAUSE / UNPAUSE / REPLY / TRACKING / TRACKINGINFO etc. —
        // tolerant OK so clients that probe defensively don't error out.
        _ => encode_error(
            out,
            &format!(
                "ERR unknown CLIENT subcommand '{}'",
                String::from_utf8_lossy(args.get(1).unwrap_or(&[][..]))
            ),
        ),
    }
}

/// `CLIENT LIST` / `CLIENT INFO` reply body — V2 wraps it in a bulk
/// string (Redis legacy); V3 wraps it in a Verbatim string tagged
/// `txt:` so a RESP3 client knows to render it as plain text without
/// quoting / escaping. Body bytes are identical.
fn emit_client_text(out: &mut Vec<u8>, body: &[u8], proto: RespVersion) {
    match proto {
        RespVersion::V2 => encode_bulk(out, body),
        RespVersion::V3 => encode_verbatim(out, *b"txt", body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn run(rest: &[&[u8]]) -> Vec<u8> {
        let mut a = Argv::default();
        a.push(b"CLIENT");
        for r in rest {
            a.push(r);
        }
        let mut out = Vec::new();
        cmd_client(&a, &mut out, RespVersion::V2);
        out
    }

    #[test]
    fn id_returns_integer() {
        let out = run(&[b"ID"]);
        assert_eq!(out, b":1\r\n");
    }

    #[test]
    fn getname_returns_empty_bulk() {
        let out = run(&[b"GETNAME"]);
        assert_eq!(out, b"$0\r\n\r\n");
    }

    #[test]
    fn setname_accepted() {
        let out = run(&[b"SETNAME", b"my-client"]);
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn setname_wrong_args() {
        let out = run(&[b"SETNAME"]);
        assert!(out.starts_with(b"-ERR"));
    }

    #[test]
    fn list_returns_bulk_with_entry() {
        let out = run(&[b"LIST"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with('$'));
        assert!(s.contains("id=1"));
        assert!(s.contains("db=0"));
    }

    #[test]
    fn kill_returns_zero() {
        let out = run(&[b"KILL", b"ADDR", b"1.2.3.4:5"]);
        assert_eq!(out, b":0\r\n");
    }

    #[test]
    fn info_returns_bulk() {
        let out = run(&[b"INFO"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with('$'));
        assert!(s.contains("id=1"));
    }

    #[test]
    fn no_evict_returns_ok() {
        let out = run(&[b"NO-EVICT", b"ON"]);
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn unknown_subcommand_errors() {
        let out = run(&[b"UNKNOWN-SUB"]);
        assert!(out.starts_with(b"-ERR"));
    }

    #[test]
    fn bare_client_errors() {
        let out = run(&[]);
        assert!(out.starts_with(b"-ERR"));
    }
}
