//! Pubsub frame vocabulary for the async subscriber — re-exported
//! from the canonical definitions in [`kevy_resp_client`], shared
//! with the blocking clients (one enum, one RESP→event classifier,
//! no per-crate mirrors).
//!
//! ```
//! use kevy_client_async::pubsub::PubsubEvent;
//! use kevy_resp::Reply;
//!
//! let frame = Reply::Array(vec![
//!     Reply::Bulk(b"message".to_vec()),
//!     Reply::Bulk(b"news".to_vec()),
//!     Reply::Bulk(b"hello".to_vec()),
//! ]);
//! let event = PubsubEvent::try_from(frame)?;
//! assert_eq!(event, PubsubEvent::Message { channel: b"news".to_vec(), payload: b"hello".to_vec() });
//! # Ok::<(), std::io::Error>(())
//! ```

pub use kevy_resp_client::PubsubEvent;
