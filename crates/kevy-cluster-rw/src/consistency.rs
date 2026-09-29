//! How fresh a read must be: [`ReadConsistency`].

/// Where [`crate::ReadWriteClient::request_read`] may serve a read.
///
/// ```no_run
/// use kevy_cluster_rw::{ReadConsistency, ReadWriteClient};
///
/// let mut c = ReadWriteClient::connect(("10.0.0.11", 6004), &[("10.0.0.12", 6004)])?;
/// let get = [b"GET".to_vec(), b"k".to_vec()];
/// let maybe_stale = c.request_read(&get, ReadConsistency::Eventual)?;
/// let fresh = c.request_read(&get, ReadConsistency::Primary)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ReadConsistency {
    /// Any replica, round-robin (the primary when there are none): the
    /// value may lag the primary by the replication delay.
    #[default]
    Eventual,
    /// The primary, so the read sees every write it has acknowledged
    /// (`READCONSISTENT` semantics).
    Primary,
}
