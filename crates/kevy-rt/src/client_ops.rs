//! CLIENT-face machinery: the per-conn row renderer shared by
//! `CLIENT LIST` / `CLIENT INFO`, the `CLIENT KILL` selector, and the
//! per-shard fan-out handlers. Everything here runs on the owning
//! shard's reactor thread, where the conn table is plain data
//! (thread-per-core, no locks).

use crate::Commands;
use crate::conn::Conn;
use crate::message::Part;
use crate::shard::Shard;
use kevy_resp::ArgvView;

/// Parsed `CLIENT KILL` selector. `Addr` matches the peer `ip:port`
/// exactly; `Id` matches the instance-unique conn id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientKillFilter {
    /// Peer address (`ip:port`) equality.
    Addr(Vec<u8>),
    /// Instance-unique conn id equality.
    Id(u64),
}

impl ClientKillFilter {
    /// Parse the argv of `CLIENT KILL …`. Returns the selector plus
    /// `true` for the legacy positional form (`CLIENT KILL addr:port`),
    /// whose reply is `+OK` / `-ERR` instead of the filtered form's
    /// killed-count integer. `None` = a shape this server doesn't
    /// support (the caller answers with a syntax error).
    pub fn parse<A: ArgvView + ?Sized>(args: &A) -> Option<(Self, bool)> {
        match args.len() {
            3 => {
                let a = args.get(2)?;
                a.contains(&b':').then(|| (Self::Addr(a.to_vec()), true))
            }
            4 => {
                let kind = args.get(2)?.to_ascii_uppercase();
                let val = args.get(3)?;
                match kind.as_slice() {
                    b"ID" => std::str::from_utf8(val)
                        .ok()?
                        .parse()
                        .ok()
                        .map(|id| (Self::Id(id), false)),
                    b"ADDR" => Some((Self::Addr(val.to_vec()), false)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// Render one `CLIENT LIST` / `CLIENT INFO` row into `out`. The field
/// set mirrors the Redis 7.x shape; fields kevy keeps no per-conn
/// state for are reported at their idle defaults (`cmd=NULL` — the
/// last-command name is not tracked).
pub(crate) fn client_row(id: u64, conn: &Conn, out: &mut Vec<u8>) {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(224);
    let _ = writeln!(
        s,
        "id={id} addr={}:{} laddr=0.0.0.0:0 fd={} name={} age={} idle=0 \
         flags=N db=0 sub={} psub={} ssub=0 multi={} watch={} qbuf={} \
         qbuf-free=0 argv-mem=0 multi-mem=0 tot-mem=0 rbs=0 rbp=0 obl={} \
         oll=0 omem=0 events=r cmd=NULL user=default redir=-1 resp={} \
         lib-name= lib-ver=",
        conn.peer.0,
        conn.peer.1,
        conn.sock.raw(),
        String::from_utf8_lossy(&conn.client_name),
        conn.created.elapsed().as_secs(),
        conn.sub.len(),
        conn.psub.len(),
        conn.multi.as_ref().map_or(-1, |q| q.len() as i64),
        conn.watched.len(),
        conn.input.len(),
        conn.output.len().saturating_sub(conn.write_pos),
        match conn.proto {
            kevy_resp::RespVersion::V2 => 2,
            kevy_resp::RespVersion::V3 => 3,
        },
    );
    out.extend_from_slice(s.as_bytes());
}

impl<C: Commands> Shard<C> {
    /// `Op::ClientList` — render every real client conn on this shard
    /// (cluster-bus links excluded: infra, not clients).
    pub(crate) fn exec_client_list(&mut self) -> Part {
        let mut text = Vec::with_capacity(self.conns.len() * 192);
        for (id, conn) in &self.conns {
            if conn.cluster {
                continue;
            }
            client_row(*id, conn, &mut text);
        }
        Part::ExtensionChunk(text)
    }

    /// `Op::ClientKill` — mark every matching conn closing and hand it
    /// to the reactor's sweep (epoll: the dirty-flush close path;
    /// io_uring: the periodic closing-set reap). Teardown waits for
    /// the conn's output to drain, so a self-kill still delivers its
    /// own reply first. Returns the matched count.
    pub(crate) fn exec_client_kill(&mut self, filter: &ClientKillFilter) -> Part {
        let mut victims: Vec<u64> = Vec::new();
        for (id, conn) in &self.conns {
            if conn.cluster || conn.closing {
                continue;
            }
            let hit = match filter {
                ClientKillFilter::Id(want) => *id == *want,
                ClientKillFilter::Addr(addr) => {
                    format!("{}:{}", conn.peer.0, conn.peer.1).as_bytes() == addr.as_slice()
                }
            };
            if hit {
                victims.push(*id);
            }
        }
        for id in &victims {
            if let Some(conn) = self.conns.get_mut(id) {
                conn.closing = true;
            }
            self.dirty.push(*id);
            self.closing_uring_conns.push(*id);
        }
        Part::Int(victims.len() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::ClientKillFilter;
    use kevy_resp::Argv;

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    #[test]
    fn parse_legacy_addr_form() {
        let a = argv(&[b"CLIENT", b"KILL", b"127.0.0.1:50123"]);
        assert_eq!(
            ClientKillFilter::parse(&a),
            Some((ClientKillFilter::Addr(b"127.0.0.1:50123".to_vec()), true))
        );
    }

    #[test]
    fn parse_id_and_addr_filters() {
        let a = argv(&[b"CLIENT", b"KILL", b"ID", b"42"]);
        assert_eq!(ClientKillFilter::parse(&a), Some((ClientKillFilter::Id(42), false)));
        let a = argv(&[b"CLIENT", b"KILL", b"addr", b"10.0.0.1:1"]);
        assert_eq!(
            ClientKillFilter::parse(&a),
            Some((ClientKillFilter::Addr(b"10.0.0.1:1".to_vec()), false))
        );
    }

    #[test]
    fn parse_rejects_unsupported_shapes() {
        assert_eq!(ClientKillFilter::parse(&argv(&[b"CLIENT", b"KILL"])), None);
        assert_eq!(ClientKillFilter::parse(&argv(&[b"CLIENT", b"KILL", b"noport"])), None);
        assert_eq!(
            ClientKillFilter::parse(&argv(&[b"CLIENT", b"KILL", b"LADDR", b"1.2.3.4:5"])),
            None
        );
        assert_eq!(
            ClientKillFilter::parse(&argv(&[b"CLIENT", b"KILL", b"ID", b"notanum"])),
            None
        );
    }
}
