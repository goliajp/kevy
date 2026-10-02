//! Recording a write as something other than its argv: the cold side of
//! [`Shard::post_write_housekeeping`], split from [`crate::exec_dispatch`]
//! for the 500-line rule.

use kevy_resp::{Argv, ArgvView};

use crate::message::DispatchMeta;
use crate::shard::Shard;
use crate::{BlockKind, Commands};

impl<C: Commands> Shard<C> {
    /// Cold sibling of the AsIs arm in [`Self::post_write_housekeeping`]:
    /// record a `Replace` effect frame to the AOF + replication backlog
    /// (same gates as the AsIs path); for a `Suppress`, record the frames
    /// a deferred effect describes, built only when there is somewhere to
    /// record them, else nothing. Out-of-line — only verbs whose effect
    /// the argv does not pin down (SPOP, a generated XADD ID, a claim)
    /// land here.
    #[cold]
    #[inline(never)]
    pub(crate) fn record_propagation_override<A: ArgvView + ?Sized>(
        &mut self,
        prop: crate::propagation::Propagate,
        args: &A,
    ) {
        if let crate::propagation::Propagate::Replace(frame) = prop {
            let total: usize = frame.iter().map(Vec::len).sum();
            let mut argv = Argv::with_capacity(frame.len(), total);
            for part in &frame {
                argv.push(part);
            }
            self.record_frame(&argv);
            return;
        }
        let Some(effect) = crate::propagation::take_deferred() else {
            return; // Suppress: nothing recorded, nothing pushed.
        };
        let replicating =
            self.replicate.is_some() && !crate::replication_gate::is_applying_replicated();
        if self.aof.is_none() && !replicating {
            return;
        }
        for frame in kevy_verbs::aof::deferred_frames(&self.store, args, &effect) {
            self.record_frame(&frame);
        }
    }

    /// Record a command that ran outside the dispatch path — a parked
    /// blocking command served once its key got data, or the undo that
    /// puts a cross-shard serve's element back — the way the dispatch
    /// path records a write: AOF, replicas, WATCH, index upkeep. `ran`
    /// says whether it answered anything; one that did not (the data was
    /// already gone) changed nothing, and whatever record it armed is
    /// dropped rather than left for the next command on this thread.
    pub(crate) fn record_served<A: ArgvView + ?Sized>(
        &mut self,
        argv: &A,
        key_idx: usize,
        ran: bool,
    ) {
        if !ran || !self.commands.is_write(argv) {
            crate::propagation::discard_override();
            return;
        }
        let meta = DispatchMeta {
            is_write: true,
            wake_idx: None,
            key_idx: u8::try_from(key_idx).ok(),
            verb: crate::VerbId::UNKNOWN,
            key_hash: 0,
        };
        self.post_write_housekeeping(argv, meta);
    }

    /// Record what a command asked to be recorded, for a command that is
    /// not recorded by its argv: a blocking command that parked (a group
    /// read creates its consumer before it waits), or a fan-out whose
    /// reduce changed state kept outside the store. Left armed, the next
    /// write on this thread would take it as its own and lose its own
    /// record.
    pub(crate) fn record_armed<A: ArgvView + ?Sized>(&mut self, args: &A) {
        if !crate::propagation::take_armed() {
            return;
        }
        // a parked command has nothing to announce yet
        crate::propagation::take_notify();
        match crate::propagation::take_override() {
            crate::propagation::Propagate::AsIs => {}
            prop => self.record_propagation_override(prop, args),
        }
    }

    fn record_frame(&mut self, argv: &Argv) {
        if self.aof.is_some() {
            self.log_write(argv);
        }
        if let Some(src) = self.replicate.as_mut().map(|f| f.source_mut())
            && !crate::replication_gate::is_applying_replicated()
        {
            src.push_mutation(argv);
        }
    }
}

/// Where a served blocking command names its key: the stream forms end
/// `… STREAMS <key> <id>`, every other one names it first.
pub(crate) fn served_key_idx(kind: BlockKind, argc: usize) -> usize {
    match kind {
        BlockKind::XReadBlock | BlockKind::XReadGroupBlock => argc.saturating_sub(2),
        BlockKind::Bzmpop | BlockKind::Blmpop => 3,
        _ => 1,
    }
}
