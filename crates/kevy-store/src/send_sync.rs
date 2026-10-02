//! Send and Sync are part of the public contract: a change that loses
//! either for a public type fails to compile here rather than in a caller.

use crate::*;

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Store>();
    send_sync::<StoreError>();
    send_sync::<KevyError>();
    send_sync::<RenameOutcome>();
    send_sync::<EvictionPolicy>();
    send_sync::<SetCondition>();
    send_sync::<ListEnd>();
    send_sync::<InsertPosition>();
    send_sync::<ScoreCompare>();
    send_sync::<BitOp>();
    send_sync::<ExpireStats>();
    send_sync::<DetachedEntries>();
    send_sync::<HExpireCond>();
    send_sync::<KeyspaceEvent>();
    send_sync::<SnapshotView>();
    send_sync::<ZAggregate>();
    send_sync::<StreamData>();
    send_sync::<StreamId>();
    send_sync::<StreamIdError>();
    send_sync::<XAddIdSpec>();
    send_sync::<XClaimOpts>();
    send_sync::<MissingStream>();
    send_sync::<AckMode>();
    send_sync::<ClaimMode>();
    send_sync::<ConsumerGroup>();
    send_sync::<ConsumerState>();
    send_sync::<PelEntry>();
    send_sync::<GroupCreateMode>();
    send_sync::<ReadGroupId>();
    send_sync::<PendingSummary>();
    send_sync::<PendingExtended>();
    send_sync::<AutoclaimResult>();
    send_sync::<LoadedGroup>();
    send_sync::<GetReply<'static>>();
    send_sync::<GetShared>();
    send_sync::<Value>();
    send_sync::<Score>();
    send_sync::<ScoreBound>();
    send_sync::<ZaddFlags>();
    send_sync::<ZaddReport>();
    send_sync::<packed_row::PackedRow>();
    send_sync::<ZSpan<'static>>();
    send_sync::<ZRange<'static>>();
};

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<SealedRows>();
    send_sync::<SegRowsError>();
    send_sync::<TierStats>();
    send_sync::<ColdRead>();
    send_sync::<SyncColdRead>();
};
