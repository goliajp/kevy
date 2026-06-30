//! v2.0.16 — `CLIENT SETNAME` / `CLIENT GETNAME` interception
//! (closes v1.52.x finding).
//!
//! These two subcommands need per-connection state which the
//! stateless `cmd_client` dispatch in `kevy` can't access. We
//! intercept them at the reactor level — `handle_command` already
//! owns `&mut Conn` via `self.conns.get_mut(conn_id)` — and emit
//! the reply directly with `immediate_reply`.
//!
//! All other CLIENT subcommands (`ID`, `LIST`, `INFO`, `KILL`,
//! `NO-EVICT`, etc.) fall through to the standard dispatch path.
//!
//! Lives outside `exec.rs` to keep that file under the 500-LOC
//! house rule.

use kevy_resp::ArgvView;

use crate::shard::Shard;
use crate::Commands;

impl<C: Commands> Shard<C> {
    /// Return `true` when `args` is `CLIENT SETNAME <name>` or
    /// `CLIENT GETNAME` and the intercept emitted a reply.
    pub(crate) fn try_intercept_client<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
    ) -> bool {
        if args.len() < 2 {
            return false;
        }
        let Some(verb) = args.get(0) else { return false };
        if !verb.eq_ignore_ascii_case(b"CLIENT") {
            return false;
        }
        let Some(sub) = args.get(1) else { return false };
        let sub_upper = sub.to_ascii_uppercase();
        match sub_upper.as_slice() {
            b"SETNAME" => {
                if args.len() != 3 {
                    self.immediate_reply(
                        conn_id,
                        b"-ERR wrong number of arguments for 'client|setname'\r\n".to_vec(),
                    );
                    return true;
                }
                let name = args.get(2).unwrap_or(&[]);
                // Redis disallows whitespace + control bytes in the
                // name (the LIST output would be ambiguous otherwise).
                if name.iter().any(|b| b.is_ascii_whitespace() || *b < 0x20) {
                    self.immediate_reply(
                        conn_id,
                        b"-ERR Client names cannot contain spaces, newlines or special characters.\r\n".to_vec(),
                    );
                    return true;
                }
                if let Some(c) = self.conns.get_mut(&conn_id) {
                    c.client_name.clear();
                    c.client_name.extend_from_slice(name);
                }
                self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
                true
            }
            b"GETNAME" => {
                if args.len() != 2 {
                    self.immediate_reply(
                        conn_id,
                        b"-ERR wrong number of arguments for 'client|getname'\r\n".to_vec(),
                    );
                    return true;
                }
                if let Some(c) = self.conns.get(&conn_id) {
                    let name = c.client_name.clone();
                    let mut out = Vec::with_capacity(8 + name.len());
                    out.extend_from_slice(format!("${}\r\n", name.len()).as_bytes());
                    out.extend_from_slice(&name);
                    out.extend_from_slice(b"\r\n");
                    self.immediate_reply(conn_id, out);
                } else {
                    self.immediate_reply(conn_id, b"$0\r\n\r\n".to_vec());
                }
                true
            }
            _ => false,
        }
    }
}
