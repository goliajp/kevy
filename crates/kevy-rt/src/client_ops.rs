//! CLIENT-face machinery: the per-conn row renderer shared by
//! `CLIENT LIST` / `CLIENT INFO`, the `CLIENT KILL` selector, and the
//! per-shard fan-out handlers. Everything here runs on the owning
//! shard's reactor thread, where the conn table is plain data
//! (thread-per-core, no locks).

// `write!` into a `String` / `Vec` returns a `Result` because the
// trait must, not because it can fail.
#![expect(clippy::let_underscore_must_use, reason = "writing to an in-memory buffer cannot fail")]

use crate::Commands;
use crate::conn::Conn;
use crate::message::Part;
use crate::shard::Shard;
use kevy_resp::ArgvView;

/// Parsed `CLIENT KILL` selector. `Addr` matches the peer `ip:port`
/// exactly; `Id` matches the instance-unique conn id.
///
/// ```
/// use kevy_rt::{ClientKillFilter, KillReply};
///
/// let argv = kevy_resp::Argv::from(vec![b"CLIENT".to_vec(), b"KILL".to_vec(), b"ID".to_vec(), b"7".to_vec()]);
/// assert_eq!(ClientKillFilter::parse(&argv), Some((ClientKillFilter::Id(7), KillReply::Count)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClientKillFilter {
    /// Peer address (`ip:port`) equality.
    ///
    /// ```
    /// use kevy_rt::{Argv, ClientKillFilter};
    ///
    /// let argv = Argv::from(vec![b"CLIENT".to_vec(), b"KILL".to_vec(), b"ADDR".to_vec(), b"10.0.0.1:5".to_vec()]);
    /// let filter = ClientKillFilter::parse(&argv).map(|(f, _)| f);
    /// assert_eq!(filter, Some(ClientKillFilter::Addr(b"10.0.0.1:5".to_vec())));
    /// ```
    Addr(Vec<u8>),
    /// Instance-unique conn id equality.
    ///
    /// ```
    /// use kevy_rt::{Argv, ClientKillFilter};
    ///
    /// let argv = Argv::from(vec![b"CLIENT".to_vec(), b"KILL".to_vec(), b"ID".to_vec(), b"42".to_vec()]);
    /// assert_eq!(ClientKillFilter::parse(&argv).map(|(f, _)| f), Some(ClientKillFilter::Id(42)));
    /// ```
    Id(u64),
}

/// Which reply a `CLIENT KILL` form answers with.
///
/// ```
/// use kevy_rt::{ClientKillFilter, KillReply};
///
/// let argv = kevy_resp::Argv::from(vec![b"CLIENT".to_vec(), b"KILL".to_vec(), b"10.0.0.1:5".to_vec()]);
/// assert_eq!(ClientKillFilter::parse(&argv).map(|(_, r)| r), Some(KillReply::Status));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KillReply {
    /// The legacy positional form (`CLIENT KILL addr:port`): `+OK`, or
    /// `-ERR` when no connection matched.
    ///
    /// ```
    /// use kevy_rt::{Argv, ClientKillFilter, KillReply};
    ///
    /// // `CLIENT KILL 10.0.0.1:5` — the positional form answers `+OK`.
    /// let argv = Argv::from(vec![b"CLIENT".to_vec(), b"KILL".to_vec(), b"10.0.0.1:5".to_vec()]);
    /// assert_eq!(ClientKillFilter::parse(&argv).map(|(_, r)| r), Some(KillReply::Status));
    /// ```
    Status,
    /// The filtered form (`CLIENT KILL ID …` / `ADDR …`): the number of
    /// connections closed.
    ///
    /// ```
    /// use kevy_rt::{Argv, ClientKillFilter, KillReply};
    ///
    /// // `CLIENT KILL ID 7` — the filtered form answers with a count.
    /// let argv = Argv::from(vec![b"CLIENT".to_vec(), b"KILL".to_vec(), b"ID".to_vec(), b"7".to_vec()]);
    /// assert_eq!(ClientKillFilter::parse(&argv).map(|(_, r)| r), Some(KillReply::Count));
    /// ```
    Count,
}

impl ClientKillFilter {
    /// Parse the argv of `CLIENT KILL …`: the selector, and which reply
    /// its form answers with. `None` = a shape this server doesn't
    /// support (the caller answers with a syntax error).
    pub fn parse<A: ArgvView + ?Sized>(args: &A) -> Option<(Self, KillReply)> {
        match args.len() {
            3 => {
                let a = args.get(2)?;
                a.contains(&b':').then(|| (Self::Addr(a.to_vec()), KillReply::Status))
            }
            4 => {
                let kind = args.get(2)?.to_ascii_uppercase();
                let val = args.get(3)?;
                match kind.as_slice() {
                    b"ID" => std::str::from_utf8(val)
                        .ok()?
                        .parse()
                        .ok()
                        .map(|id| (Self::Id(id), KillReply::Count)),
                    b"ADDR" => Some((Self::Addr(val.to_vec()), KillReply::Count)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// Which clients a `CLIENT LIST` shows: all of them, one type, or some
/// ids.
///
/// ```
/// use kevy_rt::{Argv, ClientListFilter};
///
/// let argv = |s: &str| Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// assert_eq!(ClientListFilter::parse(&argv("CLIENT LIST")), Ok(ClientListFilter::All));
/// assert_eq!(ClientListFilter::parse(&argv("CLIENT LIST type PubSub")), Ok(ClientListFilter::PubSub));
/// assert_eq!(ClientListFilter::parse(&argv("CLIENT LIST ID 3 -1")), Ok(ClientListFilter::Ids(vec![3, -1])));
/// assert_eq!(
///     ClientListFilter::parse(&argv("CLIENT LIST TYPE foo")).unwrap_err(),
///     "ERR Unknown client type 'foo'",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClientListFilter {
    /// No filter.
    ///
    /// ```
    /// assert_ne!(kevy_rt::ClientListFilter::All, kevy_rt::ClientListFilter::Normal);
    /// ```
    All,
    /// `TYPE normal`: the clients holding no subscription.
    ///
    /// ```
    /// assert_ne!(kevy_rt::ClientListFilter::Normal, kevy_rt::ClientListFilter::PubSub);
    /// ```
    Normal,
    /// `TYPE pubsub`: the clients holding a subscription, in either
    /// protocol.
    ///
    /// ```
    /// assert_ne!(kevy_rt::ClientListFilter::PubSub, kevy_rt::ClientListFilter::Replication);
    /// ```
    PubSub,
    /// `TYPE master | replica | slave`: replication runs over links of its
    /// own, never a client connection, so these list none.
    ///
    /// ```
    /// assert_ne!(kevy_rt::ClientListFilter::Replication, kevy_rt::ClientListFilter::All);
    /// ```
    Replication,
    /// `ID id [id …]`; an id no connection has (0, a negative one) lists
    /// none.
    ///
    /// ```
    /// assert_ne!(kevy_rt::ClientListFilter::Ids(vec![1]), kevy_rt::ClientListFilter::All);
    /// ```
    Ids(Vec<i64>),
}

impl ClientListFilter {
    /// Read the argv of `CLIENT LIST …`, refusing as Redis refuses: an
    /// unknown type by name, a non-integer id, any other shape as a syntax
    /// error.
    pub fn parse<A: ArgvView + ?Sized>(args: &A) -> Result<Self, String> {
        let word = |i: usize| args.get(i).map(<[u8]>::to_ascii_uppercase);
        match (args.len(), word(2).as_deref()) {
            (2, _) => Ok(Self::All),
            (4, Some(b"TYPE")) => match word(3).as_deref() {
                Some(b"NORMAL") => Ok(Self::Normal),
                Some(b"PUBSUB") => Ok(Self::PubSub),
                Some(b"MASTER" | b"REPLICA" | b"SLAVE") => Ok(Self::Replication),
                _ => {
                    Err(format!("ERR Unknown client type '{}'", String::from_utf8_lossy(&args[3])))
                }
            },
            (4.., Some(b"ID")) => (3..args.len())
                .map(|i| std::str::from_utf8(&args[i]).ok().and_then(|s| s.parse().ok()))
                .collect::<Option<Vec<i64>>>()
                .map(Self::Ids)
                .ok_or_else(|| "ERR Invalid client ID".to_string()),
            _ => Err("ERR syntax error".to_string()),
        }
    }

    fn admits(&self, id: u64, conn: &Conn) -> bool {
        match self {
            Self::All => true,
            Self::Normal => !is_pubsub(conn),
            Self::PubSub => is_pubsub(conn),
            Self::Replication => false,
            Self::Ids(ids) => ids.iter().any(|&want| u64::try_from(want) == Ok(id)),
        }
    }
}

fn is_pubsub(conn: &Conn) -> bool {
    !conn.sub.is_empty() || !conn.psub.is_empty()
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
         flags={} db=0 sub={} psub={} ssub=0 multi={} watch={} qbuf={} \
         qbuf-free=0 argv-mem=0 multi-mem=0 tot-mem=0 rbs=0 rbp=0 obl={} \
         oll=0 omem=0 events=r cmd=NULL user=default redir=-1 resp={} \
         lib-name= lib-ver=",
        conn.peer.0,
        conn.peer.1,
        conn.sock.raw(),
        String::from_utf8_lossy(&conn.client_name),
        conn.created.elapsed().as_secs(),
        if is_pubsub(conn) { "P" } else { "N" },
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
    pub(crate) fn exec_client_list(&mut self, filter: &ClientListFilter) -> Part {
        let mut text = Vec::with_capacity(self.conns.len() * 192);
        for (id, conn) in &self.conns {
            if conn.cluster || !filter.admits(*id, conn) {
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
            // Eagerly cancel the victim's block waiters (parked
            // BLPOP/XREAD + cross-shard arbiter registrations), same
            // as the QUIT/EOF path. The io_uring reap runs on a 1/16
            // iteration throttle — without this a killed-but-unreaped
            // conn's waiter stayed live and could consume a push
            // (e.g. an LPUSH element) meant for a live client.
            self.blocked.drop_for_conn(*id);
            self.cancel_xshard_on_close(*id);
        }
        Part::Int(victims.len() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientKillFilter, KillReply};
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
            Some((ClientKillFilter::Addr(b"127.0.0.1:50123".to_vec()), KillReply::Status))
        );
    }

    #[test]
    fn parse_id_and_addr_filters() {
        let a = argv(&[b"CLIENT", b"KILL", b"ID", b"42"]);
        assert_eq!(ClientKillFilter::parse(&a), Some((ClientKillFilter::Id(42), KillReply::Count)));
        let a = argv(&[b"CLIENT", b"KILL", b"addr", b"10.0.0.1:1"]);
        assert_eq!(
            ClientKillFilter::parse(&a),
            Some((ClientKillFilter::Addr(b"10.0.0.1:1".to_vec()), KillReply::Count))
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
        assert_eq!(ClientKillFilter::parse(&argv(&[b"CLIENT", b"KILL", b"ID", b"notanum"])), None);
    }
}
