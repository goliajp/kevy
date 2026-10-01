//! `--cluster call host:port command [args]`: run one command on every node.

use super::log::{self, Level};
use super::topology::Cluster;
use crate::rcli::format::{Doubles, raw};

/// Send `argv` to each selected node and print `host:port: <raw reply>`.
pub(crate) fn run(c: &mut Cluster, argv: &[Vec<u8>]) -> u8 {
    let color = c.cfg.color;
    log::head(color, Level::Info, b">>> Calling", &[b" ", &argv.join(&b' ')[..], b"\n"].concat());
    let delim = c.opts.delims.multibulk.clone();
    let words: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    let (only_masters, only_replicas) = (c.cfg.only_masters, c.cfg.only_replicas);
    for n in &mut c.nodes {
        if (only_masters && !n.is_master()) || (only_replicas && n.is_master()) {
            continue;
        }
        let mut line = [n.shown(), b": ".to_vec()].concat();
        match n.link.request(&words) {
            Ok(reply) => raw(&reply, &mut Doubles::new(&[]), &delim, &mut line),
            Err(_) => line.extend_from_slice(b"Failed!"),
        }
        log::plain(&line);
    }
    0
}
