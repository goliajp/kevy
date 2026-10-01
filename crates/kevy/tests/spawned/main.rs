//! Tests that drive kevy as a child process. Kept apart from the in-process
//! tests: a child spawned while another test drops its data-dir lock holds that
//! lock until it execs, and an in-process restart on the same dir then fails.

#![allow(clippy::unwrap_used, clippy::panic)]

mod accept_in_turn;
mod client_setname_persistence;
mod cluster_crossslot_mget;
mod cluster_known_nodes_count;
mod concurrent_writers_overlap;
mod failover_port_base;
mod global_index_restart;
mod goredis_redispy_battle;
mod jedis_stackex_battle;
mod port_claim;
mod scope_misdirected_client_port;
mod secure_clients;
mod secure_cluster;
mod segmented_stitch_e2e;
mod uds_readiness_reactor;
