//! Moving one slot the classic way: mark it importing and migrating, MIGRATE
//! its keys in batches, then give it to the target everywhere.

use super::migrate::Progress;
use super::topology::Cluster;
use crate::rcli::send::write_out;
use kevy_resp::Reply;

/// Move `slot` from `source` to `target`; `false` after saying what failed.
pub(crate) fn move_slot(
    c: &mut Cluster,
    (source, target): (usize, usize),
    slot: u16,
    progress: Progress,
) -> bool {
    let steps = progress == Progress::Steps;
    if steps {
        write_out(&header(c, (source, target), slot));
    }
    let number = slot.to_string();
    let (source_id, target_id) = (c.nodes[source].rec.id.clone(), c.nodes[target].rec.id.clone());
    let marks = [
        (target, [&b"CLUSTER"[..], b"SETSLOT", number.as_bytes(), b"IMPORTING", &source_id]),
        (source, [&b"CLUSTER"[..], b"SETSLOT", number.as_bytes(), b"MIGRATING", &target_id]),
    ];
    for (node, argv) in marks {
        if let Some(why) = super::migrate::failure(&c.nodes[node].link.request(&argv)) {
            super::migrate::move_failed(c, node, &why);
            return false;
        }
    }
    if !move_keys(c, (source, target), number.as_bytes(), progress) {
        return false;
    }
    write_out(if steps { b"\n" } else { b"#" });
    assign(c, source, target, number.as_bytes(), &target_id);
    true
}

/// `Moving slot N from source to target: `.
fn header(c: &Cluster, (source, target): (usize, usize), slot: u16) -> Vec<u8> {
    [
        format!("Moving slot {slot} from ").as_bytes(),
        &c.nodes[source].shown(),
        b" to ",
        &c.nodes[target].shown(),
        b": ",
    ]
    .concat()
}

/// Move the keys of `slot` from `source` to `target` without changing who
/// owns the slot, reporting as a reshard does.
pub(crate) fn move_keys_only(c: &mut Cluster, (source, target): (usize, usize), slot: u16) -> bool {
    write_out(&header(c, (source, target), slot));
    if !move_keys(c, (source, target), slot.to_string().as_bytes(), Progress::Steps) {
        return false;
    }
    write_out(b"\n");
    true
}

/// GETKEYSINSLOT and MIGRATE until the slot is empty, a dot per key.
fn move_keys(
    c: &mut Cluster,
    (source, target): (usize, usize),
    slot: &[u8],
    progress: Progress,
) -> bool {
    let pipeline = c.cfg.pipeline.max(1).to_string();
    loop {
        let keys = match c.nodes[source].link.request(&[
            b"CLUSTER",
            b"GETKEYSINSLOT",
            slot,
            pipeline.as_bytes(),
        ]) {
            Ok(Reply::Array(keys)) => keys,
            other => {
                let why = super::migrate::failure(&other).unwrap_or_default();
                super::migrate::move_failed(c, source, &why);
                return false;
            }
        };
        if keys.is_empty() {
            return true;
        }
        let names: Vec<Vec<u8>> =
            keys.iter().filter_map(super::link::text).map(<[u8]>::to_vec).collect();
        if !migrate_batch(c, (source, target), &names, progress) {
            return false;
        }
    }
}

/// One MIGRATE of `keys`; on a key the target already has, compare values
/// and replace only when they match or `--cluster-replace` says so.
fn migrate_batch(
    c: &mut Cluster,
    (source, target): (usize, usize),
    keys: &[Vec<u8>],
    progress: Progress,
) -> bool {
    let reply = migrate(c, source, target, keys, false);
    match super::migrate::failure(&reply) {
        None => {}
        // The source relays the target's refusal inside its own error.
        Some(why) if why.windows(7).any(|w| w == b"BUSYKEY") => {
            write_out(b"\n*** Target key exists\n");
            if !c.cfg.replace && !super::busy_keys::same_values(c, source, target, keys) {
                return false;
            }
            write_out(b"*** Replacing target keys...\n");
            let again = migrate(c, source, target, keys, true);
            if let Some(why) = super::migrate::failure(&again) {
                super::migrate::node_error(c, source, &why);
                write_out(b"\n");
                return false;
            }
        }
        Some(why) => {
            super::migrate::move_failed(c, source, &why);
            return false;
        }
    }
    if progress == Progress::Steps {
        write_out(&vec![b'.'; keys.len()]);
    }
    true
}

fn migrate(
    c: &mut Cluster,
    source: usize,
    target: usize,
    keys: &[Vec<u8>],
    replace: bool,
) -> Result<Reply, crate::rcli::conn::LinkError> {
    let (host, port) = (c.nodes[target].rec.host.clone(), c.nodes[target].rec.port.to_string());
    let timeout = c.cfg.timeout_ms.unwrap_or(60000).to_string();
    let mut argv: Vec<&[u8]> =
        vec![b"MIGRATE", &host, port.as_bytes(), b"", b"0", timeout.as_bytes()];
    if replace {
        argv.push(b"REPLACE");
    }
    argv.push(b"KEYS");
    argv.extend(keys.iter().map(Vec::as_slice));
    c.nodes[source].link.request(&argv)
}

/// The slot belongs to the target: tell the target, the source, then every
/// other master.
fn assign(c: &mut Cluster, source: usize, target: usize, slot: &[u8], target_id: &[u8]) {
    let others =
        (0..c.nodes.len()).filter(|&i| i != source && i != target && c.nodes[i].is_master());
    let order: Vec<usize> = [target, source].into_iter().chain(others).collect();
    for i in order {
        let _ = c.nodes[i].link.request(&[b"CLUSTER", b"SETSLOT", slot, b"NODE", target_id]); // gossip carries it where this fails
    }
    c.nodes[source].rec.slots.remove(slot_number(slot));
    c.nodes[target].rec.slots.insert(slot_number(slot));
}

fn slot_number(text: &[u8]) -> u16 {
    std::str::from_utf8(text).ok().and_then(|t| t.parse().ok()).unwrap_or(0)
}
