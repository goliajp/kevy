//! `--cluster`: the cluster manager.

mod add_node;
mod addr;
mod ask;
mod call;
mod check;
mod command;
mod config;
mod create;
mod del_node;
mod join;
mod link;
mod log;
mod nodes_text;
mod owners;
mod plan;
mod set_timeout;
mod show;
mod slots;
mod table;
mod topology;

pub(crate) use command::run;
