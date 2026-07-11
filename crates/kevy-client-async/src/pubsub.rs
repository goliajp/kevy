//! Pubsub frame vocabulary for the async subscriber — re-exported
//! from the canonical definitions in [`kevy_resp_client`], shared
//! with the blocking clients (one enum, one RESP→event classifier,
//! no per-crate mirrors).

pub use kevy_resp_client::PubsubEvent;
pub(crate) use kevy_resp_client::classify_pubsub as classify;
