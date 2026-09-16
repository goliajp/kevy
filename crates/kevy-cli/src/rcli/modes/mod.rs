//! The special modes: jobs that run against the server instead of a command
//! or the REPL (`--scan`, `--bigkeys`, `--stat`, …).

mod bigkeys;
pub(crate) mod dispatch;
mod hdr;
mod hotkeys;
mod human;
mod keystats;
mod keystats_report;
mod pages;
mod progress;
mod reconnect;
mod scan;
mod server;
mod sizes;
mod stat;
mod wire;
