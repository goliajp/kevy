//! `SELECT` / `FLUSHALL` / `SWAPDB` interception — the multi-db admin verbs.
//!
//! Like `CLIENT SETNAME`, these need per-connection state (`Conn::db`) or
//! whole-shard state (`DbStores`), which the stateless dispatch in `kevy`
//! can't reach — so they're intercepted at the reactor level where
//! `handle_command` already owns `&mut Conn` and `&mut Shard`. See
//! `docs/multi-db.md`.
//!
//! `DBSIZE` needs no interception (routed via the `Deref` cursor); `FLUSHDB`
//! is intercepted only to fan the flush out to every shard.

use kevy_resp::ArgvView;

use crate::Commands;
use crate::shard::Shard;

/// Parse a `SELECT`/`SWAPDB` index the way Redis does. Returns
/// `Err(())` for a non-numeric argument ("value is not an integer or out
/// of range") and `Ok(None)` for a numeric one outside `0..databases`
/// ("DB index is out of range" — negatives included, as in Redis).
fn parse_index(b: &[u8], databases: u16) -> Result<Option<u16>, ()> {
    let s = std::str::from_utf8(b).map_err(|_| ())?;
    let v: i64 = s.parse().map_err(|_| ())?;
    if v < 0 || v >= databases as i64 { Ok(None) } else { Ok(Some(v as u16)) }
}

impl<C: Commands> Shard<C> {
    /// Return `true` when `args` is `SELECT`/`FLUSHALL`/`SWAPDB` and the
    /// intercept emitted a reply.
    pub(crate) fn try_intercept_db_admin<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
    ) -> bool {
        let Some(verb) = args.get(0) else { return false };
        // The write-class admin verbs must respect the role gate (a replica
        // refuses writes) — the intercept below runs before start_command's
        // write_denied check, so gate them here.
        if (verb.eq_ignore_ascii_case(b"FLUSHDB")
            || verb.eq_ignore_ascii_case(b"FLUSHALL")
            || verb.eq_ignore_ascii_case(b"SWAPDB"))
            && let Some(err) = self.commands.write_denied()
        {
            self.immediate_reply(conn_id, err);
            return true;
        }
        if verb.eq_ignore_ascii_case(b"SELECT") {
            return self.intercept_select(conn_id, args);
        }
        if verb.eq_ignore_ascii_case(b"FLUSHDB") {
            if args.len() > 2
                || (args.len() == 2
                    && !(args[1].eq_ignore_ascii_case(b"ASYNC")
                        || args[1].eq_ignore_ascii_case(b"SYNC")))
            {
                self.immediate_reply(conn_id, b"-ERR syntax error\r\n".to_vec());
                return true;
            }
            let db = self.conns.get(&conn_id).map(|c| c.db).unwrap_or(0);
            self.fan_out_db_admin(crate::message::DbAdminOp::FlushDb(db));
            self.store.set_cur(db);
            self.store.store_mut(db).flushall();
            self.store.store_mut(db).bump_all_watched();
            self.commands.on_flush(&mut self.store);
            self.maybe_notify_flush();
            // Durability: the admin verbs are intercepted before dispatch, so
            // they never reach post_write_housekeeping — log the frame here
            // (log() prefixes a SELECT marker when the db changed, multi-DB).
            self.log(args);
            // Replication: same frame to the feed (push_feed prefixes the
            // SELECT marker the same way) so replicas flush the same db.
            self.push_feed(args);
            self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            return true;
        }
        if verb.eq_ignore_ascii_case(b"FLUSHALL") {
            if args.len() > 2
                || (args.len() == 2
                    && !(args[1].eq_ignore_ascii_case(b"ASYNC")
                        || args[1].eq_ignore_ascii_case(b"SYNC")))
            {
                self.immediate_reply(conn_id, b"-ERR syntax error\r\n".to_vec());
                return true;
            }
            self.fan_out_db_admin(crate::message::DbAdminOp::FlushAll);
            self.store.flush_all();
            self.commands.on_flush(&mut self.store);
            for db in 0..self.store.databases() {
                self.notify_flushdb_event(db);
            }
            // Feed contract (same as exec_op's Op::Flush): a flush reaches
            // replicas through a generation bump — their cursors fall
            // behind the new generation and resync via a (per-db) snapshot
            // ship. Pushing a FLUSHALL frame would put a record at the
            // offset the bump just reset to zero.
            if let Some(f) = self.replicate.as_mut() {
                f.bump_generation();
                // Multi-DB: force a `SELECT <db>` marker on the next
                // pushed frame — after a resync the replica's apply cursor
                // restarts at db 0, so a bare frame could land there.
                self.feed_last_db.set(u16::MAX);
                let g = f.generation();
                if let Err(e) = kevy_persist::feed_meta::write_feed_gen(&self.data_dir, self.id, g)
                {
                    eprintln!("kevy: shard {} feed gen write failed: {e}", self.id);
                }
            }
            self.log(args);
            self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            return true;
        }
        if verb.eq_ignore_ascii_case(b"SWAPDB") {
            return self.intercept_swapdb(conn_id, args);
        }
        false
    }

    fn intercept_select<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) -> bool {
        if args.len() != 2 {
            self.immediate_reply(
                conn_id,
                b"-ERR wrong number of arguments for 'select' command\r\n".to_vec(),
            );
            return true;
        }
        let n = self.store.databases();
        let idx = match parse_index(&args[1], n) {
            Err(()) => {
                self.immediate_reply(
                    conn_id,
                    b"-ERR value is not an integer or out of range\r\n".to_vec(),
                );
                return true;
            }
            Ok(None) => {
                self.immediate_reply(conn_id, b"-ERR DB index is out of range\r\n".to_vec());
                return true;
            }
            Ok(Some(i)) => i,
        };
        self.store.select(idx);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.db = idx;
        }
        self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
        true
    }

    fn intercept_swapdb<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) -> bool {
        if args.len() != 3 {
            self.immediate_reply(
                conn_id,
                b"-ERR wrong number of arguments for 'swapdb' command\r\n".to_vec(),
            );
            return true;
        }
        let n = self.store.databases();
        let (a, b) = match (parse_index(&args[1], n), parse_index(&args[2], n)) {
            (Err(()), _) | (_, Err(())) => {
                self.immediate_reply(
                    conn_id,
                    b"-ERR value is not an integer or out of range\r\n".to_vec(),
                );
                return true;
            }
            (Ok(None), _) | (_, Ok(None)) => {
                self.immediate_reply(conn_id, b"-ERR DB index is out of range\r\n".to_vec());
                return true;
            }
            (Ok(Some(a)), Ok(Some(b))) => (a, b),
        };
        self.fan_out_db_admin(crate::message::DbAdminOp::SwapDb(a, b));
        self.store.swap(a, b);
        self.log(args);
        self.push_feed(args);
        self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
        true
    }

    /// `MOVE key db` entry point (`Route::Move`): validate, then run on the
    /// key's owning shard — inline when local, forwarded otherwise (the
    /// forwarded copy re-enters through `run_dispatch`, which intercepts
    /// MOVE into [`Self::exec_move`]).
    pub(crate) fn start_move<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        proto: kevy_resp::RespVersion,
        args: &A,
        meta: crate::message::DispatchMeta,
        shard: usize,
        is_quit: bool,
    ) {
        use crate::message::{Part, SmallReply};
        let err = |m: &'static [u8]| Part::Reply(SmallReply::from_vec(m.to_vec()));
        if args.len() != 3 {
            self.push_pending_single(conn_id, is_quit);
            self.fold(conn_id, seq, err(b"-ERR wrong number of arguments for 'move' command\r\n"));
            return;
        }
        let n = self.store.databases();
        match parse_index(&args[2], n) {
            Err(()) => {
                self.push_pending_single(conn_id, is_quit);
                self.fold(conn_id, seq, err(b"-ERR value is not an integer or out of range\r\n"));
                return;
            }
            Ok(None) => {
                self.push_pending_single(conn_id, is_quit);
                self.fold(conn_id, seq, err(b"-ERR DB index is out of range\r\n"));
                return;
            }
            Ok(Some(_)) => {}
        }
        self.push_pending_single(conn_id, is_quit);
        if shard == self.id {
            self.store.set_cur(meta.db);
            let part = self.exec_move(args, meta);
            if meta.is_write {
                self.post_write_housekeeping(args, meta);
            }
            self.fold(conn_id, seq, part);
        } else {
            self.forward_to(shard, conn_id, seq, args, proto, meta);
        }
    }

    /// `MOVE key db` — move one key (with its TTL) from the connection's
    /// current db to another db. The key routes to its owning shard like any
    /// keyed command (`Route::Single(1)`); this runs there via
    /// `run_dispatch`, which is why it lives in the rt and not the embedded
    /// dispatch (that only sees the current db's store).
    pub(crate) fn exec_move<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        meta: crate::message::DispatchMeta,
    ) -> crate::message::Part {
        use crate::message::{Part, SmallReply};
        let err = |m: &'static [u8]| Part::Reply(SmallReply::from_vec(m.to_vec()));
        if args.len() != 3 {
            return err(b"-ERR wrong number of arguments for 'move' command\r\n");
        }
        let n = self.store.databases();
        let to_db = match parse_index(&args[2], n) {
            Err(()) => return err(b"-ERR value is not an integer or out of range\r\n"),
            Ok(None) => return err(b"-ERR DB index is out of range\r\n"),
            Ok(Some(d)) => d,
        };
        if to_db == meta.db {
            return err(b"-ERR source and destination objects are the same\r\n");
        }
        let key: Vec<u8> = args[1].to_vec();
        // Source db is the cursor (meta.db was set by the dispatcher).
        let moved = if self.store.store_mut(to_db).key_exists(&key) {
            false
        } else {
            match self.store.take_with_ttl(&key) {
                Some((v, ttl)) => {
                    self.store.store_mut(to_db).put_with_ttl(key.clone(), v, ttl);
                    true
                }
                None => false,
            }
        };
        if moved {
            // WATCH invalidation on both dbs (the key left one and landed
            // in the other). Durability + replication ride the callers'
            // post_write_housekeeping (AOF log + feed push), like every
            // other write.
            self.store.bump_if_watched(&key);
            self.store.store_mut(to_db).bump_if_watched(&key);
        }
        Part::Int(i64::from(moved))
    }

    /// Broadcast a whole-instance admin op to every other shard (each shard
    /// owns a slice of every db's keyspace, so FLUSH*/SWAPDB must land
    /// everywhere). Fire-and-forget: the origin applies locally and replies.
    fn fan_out_db_admin(&mut self, op: crate::message::DbAdminOp) {
        for s in 0..self.nshards {
            if s != self.id {
                self.send_to(s, crate::message::Inbound::DbAdmin(op));
            }
        }
    }
}
