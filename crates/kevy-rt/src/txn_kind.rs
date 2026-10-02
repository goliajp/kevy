//! How a command takes part in a transaction.

/// Transaction-control classification for a command.
///
/// ```
/// assert_eq!(kevy_rt::TxnKind::default(), kevy_rt::TxnKind::Other);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TxnKind {
    /// `MULTI` — opens a queue on this connection.
    ///
    /// ```
    /// let multi = kevy_rt::ResolvedCmd::new(kevy_rt::Route::Local).with_txn_kind(kevy_rt::TxnKind::Multi);
    /// assert_eq!(multi.txn_kind, kevy_rt::TxnKind::Multi);
    /// ```
    Multi,
    /// `EXEC` — runs the queue, or replies nil if a WATCH was broken.
    ///
    /// ```
    /// let exec = kevy_rt::ResolvedCmd::new(kevy_rt::Route::Local).with_txn_kind(kevy_rt::TxnKind::Exec);
    /// assert_ne!(exec.txn_kind, kevy_rt::TxnKind::Multi);
    /// ```
    Exec,
    /// `DISCARD` — drops the queue and any WATCH set.
    ///
    /// ```
    /// let txn = |verb: &str| if verb == "DISCARD" { kevy_rt::TxnKind::Discard } else { kevy_rt::TxnKind::Other };
    /// assert_eq!(txn("DISCARD"), kevy_rt::TxnKind::Discard);
    /// ```
    Discard,
    /// `WATCH` — outside MULTI runs the fan-out; inside MULTI is rejected
    /// with an error (Redis semantics: `WATCH inside MULTI is not allowed`).
    /// `UNWATCH` is plain [`Self::Other`] — outside MULTI it routes to
    /// [`Route::Unwatch`](crate::Route::Unwatch) (clear + OK); inside MULTI it queues as a no-op
    /// that dispatch resolves to +OK at EXEC time.
    ///
    /// ```
    /// let txn = |verb: &str| if verb == "WATCH" { kevy_rt::TxnKind::Watch } else { kevy_rt::TxnKind::Other };
    /// // UNWATCH is not in this class.
    /// assert_eq!(txn("UNWATCH"), kevy_rt::TxnKind::Other);
    /// ```
    Watch,
    /// `RESET` — returns the connection to its state when it connected,
    /// inside MULTI as well as out of it: the queue, WATCH, subscriptions,
    /// name and protocol all go.
    ///
    /// ```
    /// let txn = |verb: &str| if verb == "RESET" { kevy_rt::TxnKind::Reset } else { kevy_rt::TxnKind::Other };
    /// assert_eq!(txn("RESET"), kevy_rt::TxnKind::Reset);
    /// ```
    Reset,
    /// Everything else: queued inside MULTI, dispatched outside it.
    #[default]
    ///
    /// ```
    /// // A plain command resolves here unless told otherwise.
    /// assert_eq!(kevy_rt::ResolvedCmd::new(kevy_rt::Route::Single(1)).txn_kind, kevy_rt::TxnKind::Other);
    /// ```
    Other,
}
