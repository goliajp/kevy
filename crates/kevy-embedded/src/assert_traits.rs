//! Compile-time proof of which public types cross threads: a type that
//! loses `Send` or `Sync` fails the build here, not in a caller's.

use crate::*;

const fn send_sync<T: Send + Sync>() {}

const _: () = {
    send_sync::<Store>();
    send_sync::<WeakStore>();
    send_sync::<Config>();
    send_sync::<TtlReaperMode>();
    send_sync::<CopyMode>();
    send_sync::<KevyInfo>();
    send_sync::<KevyTierInfo>();
    send_sync::<KevyTierCompression>();
    send_sync::<OpenReport>();
    send_sync::<ReconcileReport>();
    send_sync::<Snapshot>();
    send_sync::<SnapshotEntry>();
    send_sync::<PubsubEvent>();
    send_sync::<Subscription>();
    send_sync::<Pipeline>();
};

#[cfg(feature = "tier")]
const _: () = send_sync::<TierBudgetSpec>();

#[cfg(feature = "persist")]
const _: () = send_sync::<ReplayMode>();

#[cfg(feature = "replicate")]
const _: () = send_sync::<LinkKeys>();

#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
const _: () = {
    send_sync::<Change>();
    send_sync::<ChangeBatch>();
    send_sync::<FeedError>();
    send_sync::<FeedPosition>();
    send_sync::<PrefixInfo>();
};

#[cfg(feature = "index")]
const _: () = {
    send_sync::<IdxAdvice>();
    send_sync::<ScalarPage>();
    send_sync::<ScalarQueryOpts<'static>>();
    send_sync::<ValueFilter<'static>>();
    send_sync::<SortOrder>();
};

#[cfg(feature = "text")]
const _: () = {
    send_sync::<MatchOpts<'static>>();
    send_sync::<MatchPage>();
    send_sync::<TokenPositions>();
};
