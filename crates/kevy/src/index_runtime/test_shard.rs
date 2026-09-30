//! One shard driven the way the runtime drives it: commands through
//! dispatch, fan-out verbs through the extension op and reduce (every
//! follow-up phase included), global-index deltas handed back to the
//! shard, and ticks until the builds finish.

use kevy_resp::RespVersion;
use kevy_rt::{Argv, Commands, ExtensionReduced};
use kevy_store::Store;

use crate::KevyCommands;

pub(crate) struct Shard {
    pub(crate) cmds: KevyCommands,
    pub(crate) store: Store,
}

pub(crate) fn words(line: &str) -> Vec<Vec<u8>> {
    line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
}

impl Shard {
    pub(crate) fn new() -> Self {
        Shard { cmds: KevyCommands::new(), store: Store::new() }
    }

    /// A shard over an explicit state (a shard count or a config other
    /// than the embedded default).
    pub(crate) fn with_state(state: crate::RuntimeState) -> Self {
        Shard { cmds: KevyCommands::with_state(std::sync::Arc::new(state)), store: Store::new() }
    }

    pub(crate) fn run(&mut self, line: &str) -> Vec<u8> {
        self.cmds.dispatch(&mut self.store, &Argv::from(words(line)))
    }

    /// `run`, asserting the reply is `+OK`.
    pub(crate) fn ok(&mut self, line: &str) {
        assert_eq!(String::from_utf8_lossy(&self.run(line)), "+OK\r\n", "{line}");
    }

    /// A fan-out verb's reply: the shard's half, then the origin's,
    /// phase after phase.
    pub(crate) fn ext(&mut self, line: &str) -> Vec<u8> {
        let mut argv = words(line);
        loop {
            let chunk = self.cmds.extension_op(&mut self.store, &argv);
            match self.cmds.extension_reduce(&argv, vec![chunk], RespVersion::V2) {
                ExtensionReduced::Reply(r) => return r,
                ExtensionReduced::Continue(next) => argv = next,
                _ => panic!("{line}: a reduce this shard cannot follow"),
            }
        }
    }

    pub(crate) fn hset(&mut self, key: &str, pairs: &[(&str, &str)]) {
        let pairs: Vec<(&[u8], &[u8])> =
            pairs.iter().map(|(f, v)| (f.as_bytes(), v.as_bytes())).collect();
        self.store.hset(key.as_bytes(), &pairs).unwrap();
        self.cmds.on_write(&mut self.store, key.as_bytes());
        self.deliver();
    }

    /// Ticks until every build has finished, delivering the deltas the
    /// shard's global indexes send to themselves.
    pub(crate) fn settle(&mut self) {
        for _ in 0..16 {
            self.cmds.on_shard_tick(&mut self.store);
            self.deliver();
        }
    }

    fn deliver(&mut self) {
        for (_, payload) in self.cmds.take_ext_out() {
            self.cmds.apply_ext(&mut self.store, &payload);
        }
    }
}

pub(crate) fn text(reply: &[u8]) -> String {
    String::from_utf8_lossy(reply).into_owned()
}

/// Per index on this shard: whether it runs a window, and whether it keeps
/// a cold text directory.
pub(crate) fn window_states(ctx: &crate::state::Ctx<'_>) -> Vec<(Vec<u8>, (bool, bool))> {
    let st = ctx.shard.indexes.borrow();
    let state = |si: &super::ShardIndex| (si.window.is_some(), si.cold_text.is_some());
    st.idx.iter().map(|si| (si.spec.name().to_vec(), state(si))).collect()
}
