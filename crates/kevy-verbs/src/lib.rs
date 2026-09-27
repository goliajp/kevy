//! The command layer shared by kevy's server and its embedded engine.
//!
//! [`args`] reads argv tokens the way Redis does, and [`reply`] writes
//! RESP replies with Redis's wording. Both faces go through these, so a
//! refusal reads the same whether it came over the network or from an
//! in-process call.
//!
//! ```
//! use kevy_verbs::{args, reply};
//!
//! let mut buf = [0u8; 32];
//! assert_eq!(args::upper_verb(b"hset", &mut buf), b"HSET");
//!
//! let mut out = Vec::new();
//! reply::wrong_args(&mut out, "hset");
//! assert_eq!(out, b"-ERR wrong number of arguments for 'hset' command\r\n");
//! ```

pub mod args;
pub mod reply;
