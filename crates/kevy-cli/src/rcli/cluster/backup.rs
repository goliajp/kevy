//! `--cluster backup host:port dir`: every master's snapshot, and the
//! cluster's layout as nodes.json.

use super::log::{self, Level};
use super::topology::{Cluster, Node};
use std::os::unix::ffi::OsStrExt;

/// Run `backup`; the exit code.
pub(crate) fn run(c: &mut Cluster, dir: &[u8]) -> u8 {
    let errors = super::check::count_errors(c, super::check::Listing::Show);
    let healthy = errors == 0;
    let color = c.cfg.color;
    if !std::path::Path::new(std::ffi::OsStr::from_bytes(dir)).is_dir() {
        let text =
            [&b"[ERR] The specified backup directory '"[..], dir, b"' does not exist."].concat();
        log::line(color, Level::Err, &text);
        log::line(color, Level::Err, b"[ERR] Failed to back cluster!");
        return 1;
    }
    for i in 0..c.nodes.len() {
        if c.nodes[i].is_master() && !save(c, i, dir) {
            log::line(color, Level::Err, b"[ERR] Failed to back cluster!");
            return 1;
        }
    }
    let path = [dir, b"/nodes.json"].concat();
    log::plain(&[&b"Saving cluster configuration to: "[..], &path].concat());
    let json = nodes_json(&c.nodes, errors);
    if let Err(e) = std::fs::write(std::ffi::OsStr::from_bytes(&path), json) {
        let why = crate::rcli::conn::strerror(&e);
        log::line(color, Level::Err, &[&b"[ERR] "[..], why.as_bytes()].concat());
        log::line(color, Level::Err, b"[ERR] Failed to back cluster!");
        return 1;
    }
    if !healthy {
        log::line(color, Level::Warn, b"*** Cluster seems to have some problems, please be aware of it if you're going to restore this backup.");
    }
    log::line(color, Level::Ok, &[&b"[OK] Backup created into: "[..], dir].concat());
    0
}

fn save(c: &mut Cluster, i: usize, dir: &[u8]) -> bool {
    let n = &c.nodes[i];
    log::line(
        c.cfg.color,
        Level::Info,
        &[&b">>> Node "[..], &n.shown(), b" -> Saving RDB..."].concat(),
    );
    let file = [
        dir,
        b"/redis-node-",
        &n.rec.host,
        format!("-{}-", n.rec.port).as_bytes(),
        &n.rec.id,
        b".rdb",
    ]
    .concat();
    match crate::rcli::modes::rdb::save(&mut c.nodes[i].link, &file) {
        Ok(()) => true,
        Err(why) => {
            crate::rcli::session::eprint_bytes(&[&why, b"\n"]);
            false
        }
    }
}

/// The layout, one object per loaded node, in redis-cli's format.
pub(crate) fn nodes_json(nodes: &[Node], errors: usize) -> Vec<u8> {
    let objects: Vec<String> = nodes.iter().map(|n| node_json(n, errors)).collect();
    format!("[\n{}\n]", objects.join(",\n")).into_bytes()
}

fn node_json(n: &Node, errors: usize) -> String {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let replicate =
        n.rec.master.as_ref().map_or("null".to_string(), |m| format!("\"{}\"", text(m)));
    let slots: Vec<String> =
        n.rec.slots.ranges().iter().map(|(a, b)| format!("[{a},{b}]")).collect();
    let flags = if n.is_master() { "master" } else { "slave" };
    let mut fields = vec![
        format!("\"name\": \"{}\"", text(&n.rec.id)),
        format!("\"host\": \"{}\"", text(&n.rec.host)),
        format!("\"port\": {}", n.rec.port),
        format!("\"replicate\": {replicate}"),
        format!("\"slots\": [{}]", slots.join(",")),
        format!("\"slots_count\": {}", n.rec.slots.count()),
        format!("\"flags\": \"{flags}\""),
        format!("\"current_epoch\": {}", n.rec.epoch),
    ];
    if errors > 0 {
        fields.push(format!("\"cluster_errors\": {errors}"));
    }
    for (name, list) in [("migrating", &n.rec.migrating), ("importing", &n.rec.importing)] {
        if !list.is_empty() {
            let pairs: Vec<String> =
                list.iter().map(|(s, id)| format!("\"{s}\": \"{}\"", text(id))).collect();
            fields.push(format!("\"{name}\": {{{}}}", pairs.join(",")));
        }
    }
    format!("  {{\n    {}\n  }}", fields.join(",\n    "))
}
