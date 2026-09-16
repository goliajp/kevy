//! The special modes: jobs that run against the server instead of a command
//! or the REPL (`--scan`, `--bigkeys`, `--stat`, …).

mod bigkeys;
pub(crate) mod dispatch;
mod pages;
mod progress;
mod scan;
mod server;
mod sizes;
mod wire;
