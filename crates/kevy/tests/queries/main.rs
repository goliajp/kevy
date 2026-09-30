//! Text, window, view and tiered reads end to end.

#![allow(clippy::unwrap_used, clippy::panic)]

#[path = "../common/mod.rs"]
mod common;

mod text_global_bm25;
mod tier_hydration;
mod time_bounds_e2e;
mod view_e2e;
mod window_orderpath_e2e;
mod window_scalar_e2e;
mod window_text_e2e;
