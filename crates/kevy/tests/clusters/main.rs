//! Sharding, cluster mode and cross-shard commands.

#![allow(clippy::unwrap_used, clippy::panic)]

#[path = "../common/mod.rs"]
mod common;

mod bitop_cross_shard;
mod cluster;
mod cluster_client;
mod copy_cross_shard;
mod copy_replace_records;
mod list_move_cross_shard;
mod lua_cluster;
mod lua_multishard;
mod mpop_cross_shard;
mod multi_mget_ryow;
mod pipeline_order;
mod scope_misdirected_e2e;
mod scope_move_e2e;
mod sharded;
mod zalgebra_cross_shard;
