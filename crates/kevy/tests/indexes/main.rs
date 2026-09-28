//! Secondary indexes end to end: writes, advice, backfill, describe.

#[path = "../common/mod.rs"]
mod common;

mod index_e2e;
mod idx_advise_e2e;
mod index_write_path_coverage;
mod packed_row_backfill_e2e;
mod describe_e2e;
