//! The role gates' slow side: what a write or a read is told once the
//! cached gate bit says it may be refused. The fast check stays inline in
//! `commands.rs`; nearly every command meets a clear gate and never comes
//! here.

use kevy_resp::ArgvView;

use crate::state::KevyCommands;

impl KevyCommands {
    /// A raised write gate: the precise fence-ordering judge.
    #[cold]
    pub(crate) fn write_denied_gated(&self) -> Option<Vec<u8>> {
        self.state().replication.write_denied_reply(|| {
            let max_lag_ms = self.state().config().replication.min_replicas_max_lag_ms;
            self.shard_ctx().healthy_replica_count(max_lag_ms)
        })
    }

    /// A raised read gate: a stale or still-loading replica refuses reads.
    /// The staleness deadline is a time condition and the loading flag
    /// flips mid-window, so the live values are read every time.
    #[cold]
    pub(crate) fn read_denied_gated<A: ArgvView + ?Sized>(&self, args: &A) -> Option<Vec<u8>> {
        // PING / INFO / HELLO stay answerable while gated — health
        // checks and monitoring must keep working during a snapshot
        // load (and a stale replica still proves liveness). CLIENT /
        // CONFIG / SHUTDOWN stay answerable too: an operator must be
        // able to inspect connections, kill a misbehaving one, or
        // stop the node while a load is in flight. Matches the verbs
        // Redis flags loading-exempt.
        if args.get(0).is_some_and(|v| {
            v.eq_ignore_ascii_case(b"PING")
                || v.eq_ignore_ascii_case(b"INFO")
                || v.eq_ignore_ascii_case(b"HELLO")
                || v.eq_ignore_ascii_case(b"CLIENT")
                || v.eq_ignore_ascii_case(b"CONFIG")
                || v.eq_ignore_ascii_case(b"SHUTDOWN")
        }) {
            return None;
        }
        self.state().replication.read_denied_reply()
    }
}
