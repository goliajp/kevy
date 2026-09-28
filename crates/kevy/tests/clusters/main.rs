//! Sharding, cluster mode and cross-shard commands.

#[path = "../common/mod.rs"]
mod common;

mod cluster;
mod cluster_client;
mod cluster_crossslot_mget;
mod cluster_known_nodes_count;
mod failover_port_base;
mod scope_misdirected_client_port;
mod scope_misdirected_e2e;
mod scope_move_e2e;
mod secure_cluster;
mod sharded;
mod bitop_cross_shard;
mod copy_cross_shard;
mod list_move_cross_shard;
mod zalgebra_cross_shard;
mod multi_mget_ryow;
mod lua_cluster;
mod lua_multishard;
