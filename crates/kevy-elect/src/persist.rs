//! Durable `(epoch, voted_for)` storage — Raft's persistence rule
//! applied to the kevy election.
//!
//! Why this exists: a node that votes ACCEPT in epoch `e`, crashes,
//! and restarts with a zeroed memory could vote *again* in epoch `e`
//! for a different candidate — two candidates each collect "quorum"
//! and the cluster splits brain. Raft's fix is mechanical: `(epoch,
//! votedFor)` must hit stable storage **before** the vote reply (or
//! any frame carrying a bumped epoch) leaves the node.
//!
//! [`Elector`](crate::Elector) enforces the write discipline; this
//! module only defines the storage contract. The kevy server wires a
//! file-backed implementation (`<data_dir>/elect.meta`); tests and
//! diskless embedders use [`NoPersist`].
//!
//! ```
//! # use std::sync::{Arc, Mutex};
//! # use kevy_elect::ElectorPersist;
//! # #[derive(Clone, Default)]
//! # struct Mem(Arc<Mutex<(u64, Option<String>)>>);
//! # impl ElectorPersist for Mem {
//! #     fn save(&self, epoch: u64, voted_for: Option<&str>) {
//! #         *self.0.lock().unwrap() = (epoch, voted_for.map(str::to_string));
//! #     }
//! #     fn load(&self) -> (u64, Option<String>) {
//! #         self.0.lock().unwrap().clone()
//! #     }
//! # }
//! use std::time::Instant;
//! use kevy_elect::{Elector, Message, Role};
//!
//! let disk = Mem::default();
//! let mut c = Elector::new("c", vec!["a".into(), "b".into(), "c".into()], "c:6004", Role::Replica)
//!     .with_persist(Box::new(disk.clone()));
//! let offer = Message::Offer { new_epoch: 2, candidate_id: "a".into(), repl_offset: 0 };
//! assert_eq!(c.on_message("a", offer, Instant::now()).len(), 1); // voted for a
//!
//! // c restarts with blank memory; the saved vote stops a second vote in epoch 2
//! let mut c = Elector::new("c", vec!["a".into(), "b".into(), "c".into()], "c:6004", Role::Replica)
//!     .with_persist(Box::new(disk));
//! let rival = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 0 };
//! assert!(c.on_message("b", rival, Instant::now()).is_empty());
//! ```

/// Storage contract for the elector's `(epoch, voted_for)` pair.
///
/// **Synchronous semantics**: when [`ElectorPersist::save`] returns,
/// the pair is durable (implementations fsync before returning). The
/// elector calls `save` *before* emitting any ACCEPT and *before*
/// adopting or bumping an epoch — implementations must not defer the
/// write.
///
/// A file-backed implementation:
///
/// ```
/// use std::path::PathBuf;
/// use kevy_elect::ElectorPersist;
///
/// struct MetaFile(PathBuf);
///
/// impl ElectorPersist for MetaFile {
///     fn save(&self, epoch: u64, voted_for: Option<&str>) {
///         let tmp = self.0.with_extension("tmp");
///         let line = format!("{epoch} {}", voted_for.unwrap_or("-"));
///         let f = std::fs::File::create(&tmp).and_then(|mut f| {
///             std::io::Write::write_all(&mut f, line.as_bytes())?;
///             f.sync_all()
///         });
///         f.and_then(|()| std::fs::rename(&tmp, &self.0)).expect("elect.meta must be durable");
///     }
///     fn load(&self) -> (u64, Option<String>) {
///         let Ok(text) = std::fs::read_to_string(&self.0) else { return (0, None) };
///         let (epoch, voted) = text.split_once(' ').unwrap_or(("0", "-"));
///         (epoch.parse().unwrap_or(0), (voted != "-").then(|| voted.to_string()))
///     }
/// }
///
/// let dir = std::env::temp_dir().join(format!("kevy-elect-meta-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let meta = MetaFile(dir.join("elect.meta"));
/// assert_eq!(meta.load(), (0, None)); // fresh node
/// meta.save(3, Some("n2"));
/// assert_eq!(meta.load(), (3, Some("n2".to_string())));
/// std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait ElectorPersist {
    /// Persist the pair. Returning means "durable". `voted_for` is
    /// `Some(candidate_id)` when this node has cast (or is about to
    /// cast) its vote for `epoch`, `None` when it merely follows a
    /// higher epoch without voting.
    ///
    /// ```
    /// # use std::sync::{Arc, Mutex};
    /// # use kevy_elect::ElectorPersist;
    /// # #[derive(Clone, Default)]
    /// # struct Mem(Arc<Mutex<(u64, Option<String>)>>);
    /// # impl ElectorPersist for Mem {
    /// #     fn save(&self, epoch: u64, voted_for: Option<&str>) {
    /// #         *self.0.lock().unwrap() = (epoch, voted_for.map(str::to_string));
    /// #     }
    /// #     fn load(&self) -> (u64, Option<String>) {
    /// #         self.0.lock().unwrap().clone()
    /// #     }
    /// # }
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let disk = Mem::default();
    /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "c:6004", Role::Replica)
    ///     .with_persist(Box::new(disk.clone()));
    /// let offer = Message::Offer { new_epoch: 4, candidate_id: "b".into(), repl_offset: 0 };
    /// c.on_message("b", offer, Instant::now());
    /// // the vote was saved before the ACCEPT was handed back
    /// assert_eq!(disk.load(), (4, Some("b".to_string())));
    /// ```
    fn save(&self, epoch: u64, voted_for: Option<&str>);

    /// Load the most recently saved pair. `(0, None)` when nothing
    /// has ever been saved (fresh node) — the elector treats epoch 0
    /// as "no persisted state" and keeps its boot default.
    ///
    /// ```
    /// # use std::sync::{Arc, Mutex};
    /// # use kevy_elect::ElectorPersist;
    /// # #[derive(Clone, Default)]
    /// # struct Mem(Arc<Mutex<(u64, Option<String>)>>);
    /// # impl ElectorPersist for Mem {
    /// #     fn save(&self, epoch: u64, voted_for: Option<&str>) {
    /// #         *self.0.lock().unwrap() = (epoch, voted_for.map(str::to_string));
    /// #     }
    /// #     fn load(&self) -> (u64, Option<String>) {
    /// #         self.0.lock().unwrap().clone()
    /// #     }
    /// # }
    /// use kevy_elect::{Elector, Role};
    ///
    /// let disk = Mem::default();
    /// disk.save(7, None);
    /// let e = Elector::new("c", vec!["c".into()], "c:6004", Role::Replica).with_persist(Box::new(disk));
    /// assert_eq!(e.epoch(), 7); // restored, not the boot default of 1
    /// ```
    fn load(&self) -> (u64, Option<String>);
}

/// The default no-op backend: nothing survives a restart. Correct
/// for unit tests and single-node / diskless embedded deployments
/// where a restarted node re-joining an election it voted in is not
/// a reachable scenario.
///
/// ```
/// use kevy_elect::{ElectorPersist, NoPersist};
///
/// NoPersist.save(9, Some("n2"));
/// assert_eq!(NoPersist.load(), (0, None)); // nothing was kept
/// ```
#[derive(Debug)]
pub struct NoPersist;

impl ElectorPersist for NoPersist {
    fn save(&self, _epoch: u64, _voted_for: Option<&str>) {}

    fn load(&self) -> (u64, Option<String>) {
        (0, None)
    }
}
