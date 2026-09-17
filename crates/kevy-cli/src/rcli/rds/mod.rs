//! kevy's relational commands: catalog listings, queries and their plans,
//! scripts, CSV in and out, waiting and following — tools beside the
//! redis-cli half, sharing its connection.

mod advise;
mod catalog;
pub(crate) mod complete;
mod csv;
mod describe;
mod explain;
mod export_csv;
mod feed;
mod feed_start;
mod import_csv;
pub(crate) mod meta;
mod options;
mod query;
pub(crate) mod render;
pub(crate) mod route;
pub(crate) mod rows;
mod script;
mod status;
mod wait_ready;
mod watch;
