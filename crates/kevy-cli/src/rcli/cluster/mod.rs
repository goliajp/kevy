//! `--cluster`: the cluster manager.

mod add_node;
mod addr;
mod ask;
mod busy_keys;
mod call;
mod check;
mod command;
mod config;
mod create;
mod del_node;
mod join;
mod link;
mod log;
mod migrate;
mod migrate_atomic;
mod migrate_slot;
mod move_plan;
mod nodes_text;
mod owners;
mod plan;
mod rebalance;
mod reshard;
mod reshard_sources;
mod set_timeout;
mod show;
mod slots;
mod table;
mod topology;

pub(crate) use command::run;
