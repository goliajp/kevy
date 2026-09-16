//! kevy-cli's redis-cli: everything `redis-cli [options] [command]` does.
//!
//! Acceptance is `bench/cligate.py`: every case runs redis-cli and kevy-cli
//! against the same server and compares the bytes; where kevy-cli differs on
//! purpose, `bench/cligate/deviations.txt` says where and why.

mod cnum;
mod conn;
mod docs;
mod edit;
mod fmt_flat;
mod fmt_json;
mod fmt_tty;
mod format;
mod help;
mod hint_modes;
mod input;
mod oneshot;
mod opts;
mod opts_modes;
mod opts_parse;
mod prompt;
mod repl;
mod repr;
mod send;
mod session;
mod splitargs;
mod uri;

mod entry;

pub use entry::run;
