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
    ///
    /// ```
    /// use kevy_cluster_rw::{ReadConsistency, ReadWriteClient};
    /// use kevy_resp::Reply;
    /// # struct Node(u16, std::sync::Arc<std::sync::atomic::AtomicBool>, std::path::PathBuf);
    /// # impl Drop for Node { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.2);
    /// #     self.1.store(true, std::sync::atomic::Ordering::SeqCst); } }
    /// # fn node() -> Node {
    /// #     let (port, stop) = (kevy_testnet::free_port(), std::sync::Arc::default());
    /// #     let dir = std::env::temp_dir().join(format!("kevy-rw-doc-{}-{port}", std::process::id()));
    /// #     std::fs::create_dir_all(&dir).unwrap();
    /// #     let (s, d) = (std::sync::Arc::clone(&stop), dir.clone());
    /// #     std::thread::spawn(move || kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
    /// #         .bind([127, 0, 0, 1], port).shards(1).with_data_dir(d).with_aof(false).run(s));
    /// #     kevy_testnet::assert_listening(port, "kevy node");
    /// #     Node(port, stop, dir)
    /// # }
    /// # fn main() -> std::io::Result<()> {
    /// let (primary, replica) = (node(), node());
    /// let mut c = ReadWriteClient::connect(("127.0.0.1", primary.0), &[("127.0.0.1", replica.0)])?;
    /// c.request_write(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
    /// let get = [b"GET".to_vec(), b"k".to_vec()];
    /// // the replica answers, and (unreplicated here) has not seen the write
    /// assert_eq!(c.request_read(&get, ReadConsistency::Eventual)?, Reply::Nil);
    /// assert_eq!(ReadConsistency::default(), ReadConsistency::Eventual);
    /// # Ok(())
    /// # }
    /// ```
    #[default]
    Eventual,
    /// The primary, so the read sees every write it has acknowledged
    /// (`READCONSISTENT` semantics).
    ///
    /// ```
    /// use kevy_cluster_rw::{ReadConsistency, ReadWriteClient};
    /// use kevy_resp::Reply;
    /// # struct Node(u16, std::sync::Arc<std::sync::atomic::AtomicBool>, std::path::PathBuf);
    /// # impl Drop for Node { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.2);
    /// #     self.1.store(true, std::sync::atomic::Ordering::SeqCst); } }
    /// # fn node() -> Node {
    /// #     let (port, stop) = (kevy_testnet::free_port(), std::sync::Arc::default());
    /// #     let dir = std::env::temp_dir().join(format!("kevy-rw-doc-{}-{port}", std::process::id()));
    /// #     std::fs::create_dir_all(&dir).unwrap();
    /// #     let (s, d) = (std::sync::Arc::clone(&stop), dir.clone());
    /// #     std::thread::spawn(move || kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
    /// #         .bind([127, 0, 0, 1], port).shards(1).with_data_dir(d).with_aof(false).run(s));
    /// #     kevy_testnet::assert_listening(port, "kevy node");
    /// #     Node(port, stop, dir)
    /// # }
    /// # fn main() -> std::io::Result<()> {
    /// let (primary, replica) = (node(), node());
    /// let mut c = ReadWriteClient::connect(("127.0.0.1", primary.0), &[("127.0.0.1", replica.0)])?;
    /// c.request_write(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
    /// let get = [b"GET".to_vec(), b"k".to_vec()];
    /// // read-your-writes: the primary has the value
    /// assert_eq!(c.request_read(&get, ReadConsistency::Primary)?, Reply::Bulk(b"v".to_vec()));
    /// # Ok(())
    /// # }
    /// ```
    Primary,
}
