//! `--cluster <subcommand> [args]`: validate, load the cluster, run.

use super::addr::{self, Addr};
use super::config::Config;
use super::table;
use super::topology::Cluster;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// Run the cluster manager; the process exit code.
pub(crate) fn run(s: &mut Session) -> u8 {
    let words = s.opts.modes.cluster.clone().unwrap_or_default();
    let Some((name, args)) = words.split_first() else { return 1 };
    let Some(sub) = table::SUBS.iter().find(|t| t.name.as_bytes() == name.as_slice()) else {
        eprint_bytes(&[b"Unknown --cluster subcommand\n"]);
        return 1;
    };
    if !table::arity_ok(sub, args.len()) {
        eprint_bytes(&[b"[ERR] Wrong number of arguments for specified --cluster sub command\n"]);
        return 1;
    }
    let cfg = Config::from_opts(&s.opts);
    match sub.name {
        "help" => {
            write_out(&table::help());
            1
        }
        "info" | "check" => entry_command(s, cfg, sub.name, args),
        "create" => super::create::run(&s.opts, &cfg, args),
        "add-node" => super::add_node::run(&s.opts, cfg, args),
        "del-node" => super::del_node::run(&s.opts, cfg, args),
        "reshard" => super::reshard::run(&s.opts, cfg, args),
        "rebalance" => super::rebalance::run(&s.opts, cfg, args),
        "fix" => super::fix::run(&s.opts, cfg, args),
        "import" => super::import::run(&s.opts, cfg, args),
        "call" | "set-timeout" | "backup" => node_command(s, cfg, sub.name, args),
        _ => {
            eprint_bytes(&[b"kevy-cli: --cluster ", name, b" is not implemented yet\n"]);
            1
        }
    }
}

/// Subcommands addressed as `host:port` or `host port`.
fn entry_command(s: &Session, cfg: Config, name: &str, args: &[Vec<u8>]) -> u8 {
    let Some(entry) = addr::entry(args) else { return addr::report_invalid() };
    let Some(mut c) = Cluster::load(&s.opts, cfg, &entry) else { return 1 };
    super::show::info(&mut c);
    if name == "info" {
        return 0;
    }
    u8::from(!super::check::run(&mut c))
}

/// Subcommands whose first argument is `host:port` and the rest their own.
fn node_command(s: &Session, cfg: Config, name: &str, args: &[Vec<u8>]) -> u8 {
    let Some((first, rest)) = args.split_first() else { return 1 };
    let Some(entry) = addr::entry(std::slice::from_ref(first)) else {
        return addr::report_invalid();
    };
    if name == "set-timeout" {
        return set_timeout(s, cfg, &entry, rest);
    }
    let Some(mut c) = Cluster::load(&s.opts, cfg, &entry) else { return 1 };
    if name == "backup" {
        return super::backup::run(&mut c, rest.first().map_or(&b""[..], Vec::as_slice));
    }
    super::call::run(&mut c, rest)
}

fn set_timeout(s: &Session, cfg: Config, entry: &Addr, rest: &[Vec<u8>]) -> u8 {
    let ms = rest.first().map_or(0, |v| crate::rcli::cnum::atoi(v));
    if ms < 100 {
        eprint_bytes(&[b"Setting a node timeout of less than 100 milliseconds is a bad idea.\n"]);
        return 1;
    }
    let Some(mut c) = Cluster::load(&s.opts, cfg, entry) else { return 1 };
    super::set_timeout::run(&mut c, ms)
}
