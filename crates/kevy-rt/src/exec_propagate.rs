//! Recording a write as something other than its argv: the cold side of
//! [`Shard::post_write_housekeeping`], split from [`crate::exec_dispatch`]
//! for the 500-line rule.

use crate::Commands;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Cold sibling of the AsIs arm in [`Self::post_write_housekeeping`]:
    /// record a `Replace` effect frame, and any frames queued after it, to
    /// the AOF + replication backlog (same gates as the AsIs path), or
    /// record nothing (`Suppress`). Out-of-line — only verbs whose effect
    /// the argv does not pin down (SPOP, a generated XADD ID, a claim)
    /// land here.
    #[cold]
    #[inline(never)]
    pub(crate) fn record_propagation_override(&mut self, prop: crate::propagation::Propagate) {
        let crate::propagation::Propagate::Replace(frame) = prop else {
            return; // Suppress: nothing recorded, nothing pushed.
        };
        self.record_frame(&frame);
        for frame in crate::propagation::take_more_frames() {
            self.record_frame(&frame);
        }
    }

    fn record_frame(&mut self, frame: &[Vec<u8>]) {
        let total: usize = frame.iter().map(Vec::len).sum();
        let mut argv = kevy_resp::Argv::with_capacity(frame.len(), total);
        for part in frame {
            argv.push(part);
        }
        if self.aof.is_some() {
            self.log_write(&argv);
        }
        if let Some(src) = self.replicate.as_mut().map(|f| f.source_mut())
            && !crate::replication_gate::is_applying_replicated()
        {
            src.push_mutation(&argv);
        }
    }
}
