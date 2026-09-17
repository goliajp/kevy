//! Keys a MIGRATE found already on the target: replaced only when source
//! and target hold the same value, which DEBUG DIGEST-VALUE tells.

use super::topology::Cluster;
use crate::rcli::send::write_out;
use kevy_resp::Reply;

/// `true` when every key has the same value on both nodes; otherwise say
/// which differ (or why they could not be compared).
pub(crate) fn same_values(c: &mut Cluster, source: usize, target: usize, keys: &[Vec<u8>]) -> bool {
    write_out(b"*** Checking key values on both nodes...\n");
    let mut argv: Vec<&[u8]> = vec![b"DEBUG", b"DIGEST-VALUE"];
    argv.extend(keys.iter().map(Vec::as_slice));
    let mut digests = Vec::new();
    let mut failed = false;
    for node in [source, target] {
        match c.nodes[node].link.request(&argv) {
            Ok(Reply::Array(values)) => digests.push(values),
            other => {
                let why = super::migrate::failure(&other).unwrap_or_default();
                write_out(
                    &[
                        &b"Node "[..],
                        &c.nodes[node].shown(),
                        b" replied with error:\n",
                        &why,
                        b"\n",
                    ]
                    .concat(),
                );
                failed = true;
            }
        }
    }
    if failed {
        write_out(b"*** Value check failed!\n\n");
        return false;
    }
    // A key a node does not hold digests to all zeros: a key only one side
    // holds (MIGRATE may have moved part of the batch) is no collision.
    let held = |d: Option<&Reply>| {
        d.and_then(super::link::text).is_some_and(|t| t.iter().any(|&b| b != b'0'))
    };
    let differ: Vec<&Vec<u8>> = keys
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let src = digests.first().and_then(|d| d.get(*i));
            let dst = digests.get(1).and_then(|d| d.get(*i));
            held(src) && held(dst) && src != dst
        })
        .map(|(_, k)| k)
        .collect();
    if differ.is_empty() {
        return true;
    }
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
    false
}
