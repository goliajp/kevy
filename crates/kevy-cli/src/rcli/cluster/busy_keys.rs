//! Keys a MIGRATE found already on the target: replaced only when source
//! and target hold the same value, which DEBUG DIGEST-VALUE tells.

use super::topology::Cluster;
use crate::rcli::send::write_out;
use kevy_resp::Reply;

/// `true` when every key has the same value on both nodes; otherwise say
/// which differ (or why they could not be compared).
pub(crate) fn same_values(c: &mut Cluster, source: usize, target: usize, keys: &[Vec<u8>]) -> bool {
    write_out(b"*** Checking key values on both nodes...\n");
    let Some(digests) = digests(c, [source, target], keys) else {
        write_out(b"*** Value check failed!\n\n");
        return false;
    };
    // A key a node does not hold digests to all zeros: a key only one side
    // holds (MIGRATE may have moved part of the batch) is no collision.
    let held = |d: &Reply| super::link::text(d).is_some_and(|t| t.iter().any(|&b| b != b'0'));
    let differ: Vec<&Vec<u8>> = keys
        .iter()
        .zip(digests[0].iter().zip(digests[1].iter()))
        .filter(|(_, (src, dst))| held(src) && held(dst) && src != dst)
        .map(|(k, _)| k)
        .collect();
    if differ.is_empty() {
        return true;
    }
    report_differences(c, source, target, &differ);
    false
}

/// DEBUG DIGEST-VALUE of `keys` on both nodes, or `None` after printing
/// each node's refusal.
fn digests(c: &mut Cluster, nodes: [usize; 2], keys: &[Vec<u8>]) -> Option<[Vec<Reply>; 2]> {
    let mut argv: Vec<&[u8]> = vec![b"DEBUG", b"DIGEST-VALUE"];
    argv.extend(keys.iter().map(Vec::as_slice));
    let mut out: [Vec<Reply>; 2] = Default::default();
    let mut failed = false;
    for (slot, node) in out.iter_mut().zip(nodes) {
        match c.nodes[node].link.request(&argv) {
            Ok(Reply::Array(values)) => *slot = values,
            other => {
                let why = super::migrate::failure(&other).unwrap_or_default();
                let at = c.nodes[node].shown();
                write_out(&[&b"Node "[..], &at, b" replied with error:\n", &why, b"\n"].concat());
                failed = true;
            }
        }
    }
    (!failed).then_some(out)
}

fn report_differences(c: &Cluster, source: usize, target: usize, differ: &[&Vec<u8>]) {
    let head = format!(
        "*** Found {} key(s) in both source node and target node having different values.\n",
        differ.len()
    );
    let mut text = [
        head.as_bytes(),
        b"    Source node: ",
        &c.nodes[source].shown(),
        b"\n    Target node: ",
        &c.nodes[target].shown(),
        b"\n    Keys(s):\n",
    ]
    .concat();
    for key in differ {
        text.extend_from_slice(&[&b"    - "[..], key, b"\n"].concat());
    }
    text.extend_from_slice(b"Please fix the above key(s) manually and try again or relaunch the command \nwith --cluster-replace option to force key overriding.\n\n");
    write_out(&text);
}
