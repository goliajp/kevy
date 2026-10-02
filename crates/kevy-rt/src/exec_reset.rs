//! `RESET`: the connection back to the state it connected in — no queued
//! transaction, no WATCH, no subscriptions (left without a frame each), no
//! name, RESP2 — answered `+RESET`.

use kevy_resp::RespVersion;

use crate::Commands;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    pub(crate) fn do_reset(&mut self, conn_id: u64) {
        let Some(c) = self.conns.get(&conn_id) else { return };
        let subs: Vec<Vec<u8>> = c.sub.iter().cloned().collect();
        let patterns: Vec<Vec<u8>> = c.psub.iter().cloned().collect();
        if !subs.is_empty()
            && let Some((_, changed)) =
                self.apply_sub_to_conn(conn_id, &subs, false, b"unsubscribe")
        {
            self.apply_sub_to_registry(&changed, false);
        }
        if !patterns.is_empty() {
            let (_, changed) = self.apply_psub_to_conn(conn_id, &patterns, false);
            self.apply_psub_to_registry(&changed, false);
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.multi = None;
            c.multi_dirty = false;
            c.watched.clear();
            c.client_name.clear();
            c.proto = RespVersion::V2;
        }
        self.immediate_reply(conn_id, b"+RESET\r\n".to_vec());
    }
}
