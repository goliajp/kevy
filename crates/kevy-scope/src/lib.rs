//! kevy-scope — scoped multi-writer ownership table.
//!
//! Phase 3 of the v3-cluster RFC: a key prefix (`app:billing:`) is
//! declared to belong to one writer node; writes landing on any
//! other node answer `-MISDIRECTED writer is <host:port>`. This crate
//! is the pure-data "stone" layer — config-driven ownership table,
//! longest-prefix routing, MOVE-SCOPE quiesce-window state machine —
//! without any I/O. The kevy server's cement layer plugs it in.
//!
//! See `.claude/rfcs/2026-06-18-v3-cluster.md` `## Q3 resolution`
//! for the locked design (MOVE-SCOPE = option (a) quiesce-window;
//! no write-shadowing, no 2PC, no per-key MIGRATE/ASK).
//!
//! Anti-scope (don't add):
//! - Cross-scope transactions (a single MULTI/EXEC may not touch keys
//!   in two scopes; the client splits by scope).
//! - Automatic scope migration on `kevy-elect` ANNOUNCE — operator-
//!   issued only.
//! - Write-shadowing during migration; the writer quiesces, ships,
//!   and only then flips ownership.
//!
//! # Quick start (config side)
//!
//! ```rust
//! use kevy_scope::{OwnershipTable, Scope};
//!
//! let table = OwnershipTable::new(vec![
//!     Scope::new(b"app:billing:".to_vec(), "embed-billing-1".to_string())
//!         .with_fallback("server-eu-1".to_string()),
//!     Scope::new(b"app:auth:".to_vec(), "embed-auth-1".to_string()),
//! ])
//! .expect("non-overlapping prefixes");
//!
//! // Local node looks up routing for a key.
//! let routing = table.route(b"app:billing:invoice:42", "embed-billing-1");
//! assert!(routing.is_local_writer());
//!
//! // Same key arriving on the wrong node:
//! let routing = table.route(b"app:billing:invoice:42", "server-eu-1");
//! // server-eu-1 is the fallback, not the live writer — depending on
//! // F4 fallback state, routing is Misdirected or Owned. See the
//! // `route_with_fallback_state` API for the state-aware lookup.
//! assert!(matches!(routing, kevy_scope::Routing::Misdirected { .. }));
//! ```
#![forbid(unsafe_code)]

mod migration;
mod ownership;
mod routing;
mod scope;

pub use migration::{MigrationError, MigrationState, MigrationTable};
pub use ownership::{OwnershipError, OwnershipTable};
pub use routing::Routing;
pub use scope::Scope;
