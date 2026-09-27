//! Recording a write as something other than its argv: the cold side of
//! [`Shard::post_write_housekeeping`], split from [`crate::exec_dispatch`]
//! for the 500-line rule.

use kevy_resp::{Argv, ArgvView};

use crate::Commands;
use crate::shard::Shard;

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
        for frame in kevy_verbs::aof::deferred_frames(&mut self.store, args, &effect) {
            self.record_frame(&frame);
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
