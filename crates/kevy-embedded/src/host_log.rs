//! The frames a command's write records, handed to a host that keeps the
//! log itself.
//!
//! A store opened without a data directory records nothing: there is no
//! file to append to. The browser package is that store, and it keeps
//! its log in OPFS or IndexedDB instead, so it needs the frames a native
//! AOF would have received. Rebuilding them from the argv is not enough:
//! an `XADD *` records the id it chose, a group read records the
//! deliveries it made, a relative expiry records its absolute deadline.
//! [`Store::dispatch_argv_recorded`] collects what the write path records
//! while one command runs, on the calling thread.

use std::cell::RefCell;

use crate::store::Store;

thread_local! {
    // `Some` while a recorded dispatch runs on this thread
    static SINK: RefCell<Option<Vec<Vec<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// Whether a recorded dispatch is collecting this thread's writes.
pub(crate) fn active() -> bool {
    SINK.with(|s| s.borrow().is_some())
}

/// Keep one recorded frame, when a recorded dispatch is collecting.
pub(crate) fn push(parts: &[&[u8]]) {
    SINK.with(|s| {
        if let Some(frames) = s.borrow_mut().as_mut() {
            frames.push(parts.iter().map(|p| p.to_vec()).collect());
        }
    });
}

impl Store {
    /// [`Store::dispatch_argv`], and then `record` once per frame the
    /// command's write recorded, in order: the frames a native AOF would
    /// hold for it, which replay through [`Store::apply_frame`] to the
    /// same state. A read, or a write that changed nothing, records none.
    ///
    /// For a host that stores the log itself where there is no file to
    /// append to (the browser package pumps these into OPFS or
    /// IndexedDB). Only writes made on the calling thread during the call
    /// are collected.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// let store = Store::open(Config::default())?;
    /// let mut frames: Vec<Vec<Vec<u8>>> = Vec::new();
    /// let mut out = Vec::new();
    /// let argv = [b"INCRBY".to_vec(), b"n".to_vec(), b"5".to_vec()];
    /// store.dispatch_argv_recorded(&argv, &mut out, |f| {
    ///     frames.push(f.iter().map(|p| p.to_vec()).collect());
    /// });
    /// assert_eq!(out, b":5\r\n");
    /// assert_eq!(frames, [argv.to_vec()]);
    ///
    /// // the frames rebuild the write elsewhere
    /// let copy = Store::open(Config::default())?;
    /// for f in &frames {
    ///     copy.apply_frame(&kevy_persist::Argv::from(f.clone()));
    /// }
    /// assert_eq!(copy.get(b"n")?, Some(b"5".to_vec()));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn dispatch_argv_recorded(
        &self,
        argv: &[Vec<u8>],
        out: &mut Vec<u8>,
        mut record: impl FnMut(&[&[u8]]),
    ) {
        let outer = SINK.with(|s| s.replace(Some(Vec::new())));
        self.dispatch_argv(argv, out);
        let frames = SINK.with(|s| s.replace(outer)).unwrap_or_default();
        for f in &frames {
            let parts: Vec<&[u8]> = f.iter().map(Vec::as_slice).collect();
            record(&parts);
        }
    }
}
