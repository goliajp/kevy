//! Windowed and indexed reads over rows.

#![allow(clippy::unwrap_used, clippy::panic)]

mod idx_advise;
mod index_repack;
mod partition_local_only;
mod scalar_values_clauses;
mod window_narrow;
mod window_rows;
mod window_scalar;
mod window_text;
