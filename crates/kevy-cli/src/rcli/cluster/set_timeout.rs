//! `--cluster set-timeout host:port ms`: the node timeout, on every node.

use super::log::{self, Level};
use super::topology::Cluster;
use kevy_resp::Reply;

/// Set and persist `cluster-node-timeout` everywhere; always exits 0 once
/// the cluster is loaded, counting what failed.
pub(crate) fn run(c: &mut Cluster, ms: i32) -> u8 {
    let color = c.cfg.color;
    log::line(color, Level::Info, b">>> Reconfiguring node timeout in every cluster node...");
    let value = ms.to_string();
    let (mut ok, mut failed) = (0, 0);
    for n in &mut c.nodes {
        let set = n.link.request(&[b"CONFIG", b"SET", b"cluster-node-timeout", value.as_bytes()]);
        let outcome = match set {
            Ok(Reply::Error(e)) => Err(e),
            Ok(_) => match n.link.request(&[b"CONFIG", b"REWRITE"]) {
                Ok(Reply::Error(e)) => Err(e),
                Ok(_) => Ok(()),
                Err(e) => Err(e.text().into_bytes()),
            },
            Err(e) => Err(e.text().into_bytes()),
        };
        match outcome {
            Ok(()) => {
                log::line(
                    color,
                    Level::Warn,
                    &[b"*** New timeout set for ", &n.shown()[..]].concat(),
                );
                ok += 1;
            }
            Err(why) => {
                let text = [b"ERR setting node-timeout for ", &n.shown()[..], b": ", &why].concat();
                log::line(color, Level::Err, &text);
                failed += 1;
            }
        }
    }
    let summary = format!(">>> New node timeout set. {ok} OK, {failed} ERR.");
    log::line(color, Level::Info, summary.as_bytes());
    0
}
