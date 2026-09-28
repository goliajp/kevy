//! No shared file mappings on this target, so nothing ever stages: every
//! append goes to the file as it always did.

use std::io;

use crate::aof::Aof;

/// Never constructed here.
#[derive(Debug)]
pub(crate) enum Stage {}

/// Never constructed here.
#[derive(Debug)]
pub(crate) enum Mapped {}

impl std::io::Write for Mapped {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        match *self {}
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {}
    }
}

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

    pub(crate) fn stage_txn_closed(&mut self) {}

    pub(crate) fn stage_bypassed(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn unmap(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn stop_mapping(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn after_file_change(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn sync_file(&self) -> io::Result<()> {
        self.file.get_ref().sync_data()
    }

    pub(crate) fn map_handles(&self) -> Vec<MapHandle> {
        Vec::new()
    }
}

/// No mappings here, so no handle is ever made.
pub(crate) type MapHandle = std::convert::Infallible;

pub(crate) fn sync_handles(_maps: &[MapHandle]) -> io::Result<()> {
    Ok(())
}
