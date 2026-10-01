//! [`VerbId`]: the verb as the forwarding shard's resolve saw it, carried
//! to the shard that executes the command.

/// An opaque verb number a [`Commands`] implementation hands out from
/// [`Commands::resolve`] and reads back in [`Commands::dispatch_verb_into`],
/// so the shard that executes a command does not match the verb again.
///
/// The runtime never interprets the number; it only carries it with the
/// command, across shards included. [`VerbId::UNKNOWN`] means "not
/// resolved to anything special" and is what a command gets when its
/// implementation does not assign ids, or when it is dispatched from a
/// place that has no resolve result (AOF replay, a replica frame).
///
/// ```
/// use kevy_rt::VerbId;
///
/// const GET: VerbId = VerbId::new(1);
/// assert_ne!(GET, VerbId::UNKNOWN);
/// assert_eq!(GET.get(), 1);
/// assert_eq!(VerbId::default(), VerbId::UNKNOWN);
/// ```
///
/// [`Commands`]: crate::Commands
/// [`Commands::resolve`]: crate::Commands::resolve
/// [`Commands::dispatch_verb_into`]: crate::Commands::dispatch_verb_into
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VerbId(u16);

impl VerbId {
    /// No id: the executing shard resolves the verb from the argv.
    ///
    /// ```
    /// assert_eq!(kevy_rt::VerbId::UNKNOWN.get(), 0);
    /// ```
    pub const UNKNOWN: VerbId = VerbId(0);

    /// The id `n`. The implementation that hands it out decides what it
    /// means; `0` is [`VerbId::UNKNOWN`].
    ///
    /// ```
    /// const SET: kevy_rt::VerbId = kevy_rt::VerbId::new(2);
    /// assert_eq!(SET.get(), 2);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(n: u16) -> Self {
        VerbId(n)
    }

    /// The number this id carries.
    ///
    /// ```
    /// assert_eq!(kevy_rt::VerbId::new(7).get(), 7);
    /// ```
    #[inline]
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}
