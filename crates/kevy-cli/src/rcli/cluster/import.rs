//! `--cluster import host:port --cluster-from host:port`: move (or copy) every
//! key of a standalone server into the cluster, each to its slot's owner.

use super::addr::{self, Addr};
use super::config::Config;
use super::log::{self, Level};
use super::topology::Cluster;
use crate::rcli::conn::Conn;
use crate::rcli::opts::Opts;
use crate::rcli::send::write_out;
use crate::rcli::session::eprint_bytes;
use kevy_resp::Reply;

/// Run `import`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: Config, args: &[Vec<u8>]) -> u8 {
    let Some(entry) = addr::entry(args) else { return addr::report_invalid() };
    let Some(from) = cfg.from.clone() else {
        eprint_bytes(&[b"[ERR] Option '--cluster-from' is required for subcommand 'import'.\n"]);
        return 1;
    };
    let Some(source) = addr::entry(std::slice::from_ref(&from)) else {
        eprint_bytes(&[b"[ERR] Invalid --cluster-from host. You need to pass a valid address (ie. 120.0.0.1:7000).\n"]);
        return 1;
    };
    let text = [&b">>> Importing data from "[..], &source.shown(), b" to cluster ", &entry.shown()]
        .concat();
    log::line(cfg.color, Level::Info, &text);
    let Some(mut c) = Cluster::load(opts, cfg, &entry) else { return 1 };
    if !super::check::run(&mut c) {
        return 1;
    }
    let Some(mut link) = open_source(&c, &source) else { return 1 };
    let keys = match link.request(&[b"DBSIZE"]) {
        Ok(Reply::Int(n)) => n,
        _ => 0,
    };
    log::line(c.cfg.color, Level::Info, format!("*** Importing {keys} keys from DB 0").as_bytes());
    u8::from(!copy_all(&mut c, &mut link, &source))
}

/// Connect and authenticate to the source, which must not be a cluster node.
fn open_source(c: &Cluster, source: &Addr) -> Option<Conn> {
    let mut link = match Conn::tcp(&source.host, source.port, c.opts.connect_timeout) {
        Ok(link) => link,
        Err(why) => {
            let at = source.shown();
            eprint_bytes(&[b"Could not connect to Redis at ", &at, b": ", why.as_bytes(), b".\n"]);
            return None;
        }
    };
    if let Some(pass) = &c.cfg.from_pass {
        let reply = match &c.cfg.from_user {
            Some(user) => link.request(&[b"AUTH", user, pass]),
            None => link.request(&[b"AUTH", pass]),
        };
        if let Some(why) = super::migrate::failure(&reply) {
            source_error(source, &why);
            return None;
        }
    }
    match link.request(&[b"INFO", b"cluster"]) {
        Ok(Reply::Error(why)) => source_error(source, &why),
        Err(e) => source_error(source, e.text().as_bytes()),
        Ok(info) if super::create::field(&info, b"cluster_enabled:") == Some(b"1") => {
            log::line(
                c.cfg.color,
                Level::Err,
                b"[ERR] The source node should not be a cluster node.",
            );
        }
        Ok(_) => return Some(link),
    }
    None
}

fn source_error(source: &Addr, why: &[u8]) {
    write_out(&[&b"Source "[..], &source.shown(), b" replied with error:\n", why, b"\n"].concat());
}

/// SCAN the source and MIGRATE each key to its slot's owner.
fn copy_all(c: &mut Cluster, link: &mut Conn, source: &Addr) -> bool {
    let mut cursor = b"0".to_vec();
    loop {
        let Ok(Reply::Array(page)) = link.request(&[b"SCAN", &cursor, b"COUNT", b"1000"]) else {
            return false;
        };
        let (Some(next), Some(Reply::Array(keys))) =
            (page.first().and_then(super::link::text), page.get(1))
        else {
            return false;
        };
        cursor = next.to_vec();
        for key in keys.iter().filter_map(super::link::text) {
            if !copy_one(c, link, source, key) {
                return false;
            }
        }
        if cursor == b"0" {
            return true;
        }
    }
}

fn copy_one(c: &mut Cluster, link: &mut Conn, source: &Addr, key: &[u8]) -> bool {
    let slot = kevy_hash::key_hash_slot(key);
    let Some(owner) = c.nodes.iter().position(|n| n.is_master() && n.rec.slots.contains(slot))
    else {
        return false;
    };
    let (host, port) = (c.nodes[owner].rec.host.clone(), c.nodes[owner].rec.port.to_string());
    write_out(&[&b"Migrating "[..], key, b" to ", &c.nodes[owner].shown(), b": "].concat());
    let timeout = c.cfg.timeout_ms.unwrap_or(60000).to_string();
    let mut argv: Vec<&[u8]> =
        vec![b"MIGRATE", &host, port.as_bytes(), key, b"0", timeout.as_bytes()];
    let (auth_user, auth_pass) = (c.opts.user.clone(), c.opts.auth.clone());
    if let Some(pass) = &auth_pass {
        match &auth_user {
            Some(user) => argv.extend([&b"AUTH2"[..], user, pass]),
            None => argv.extend([&b"AUTH"[..], pass]),
        }
    }
    if c.cfg.copy {
        argv.push(b"COPY");
    }
    if c.cfg.replace {
        argv.push(b"REPLACE");
    }
    match super::migrate::failure(&link.request(&argv)) {
        None => {
            write_out(b"OK\n");
            true
        }
        Some(why) => {
            source_error(source, &why);
            false
        }
    }
}
