//! No shared file mappings on this target, so nothing ever stages: every
//! append goes to the file as it always did.

use std::io;

use crate::aof::Aof;

/// Never constructed here.
#[derive(Debug)]
pub(crate) enum Stage {}

impl Aof {
    pub(crate) fn stage_record(
        &mut self,
        _len: usize,
        _fill: impl Fn(&mut [u8]),
    ) -> io::Result<bool> {
        Ok(false)
    }

    pub(crate) fn drain_stage(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn rebase_stage(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn stage_txn_closed(&mut self) {}

    pub(crate) fn stage_bypassed(&mut self) -> io::Result<()> {
        Ok(())
    }
}
