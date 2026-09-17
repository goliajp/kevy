//! `create`'s plan, before anything is sent: which nodes are masters, which
//! slots each gets, and which master each replica follows.
//!
//! Pure, so the plan is testable without a cluster.

use super::slots::SLOTS;

/// One node to create the cluster from, by index into the given list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    pub(crate) node: usize,
    /// `(first, last)` for a master.
    pub(crate) slots: Option<(u16, u16)>,
    /// The master's node index, for a replica.
    pub(crate) replicates: Option<usize>,
}

/// What the planning said along the way, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Note {
    /// `Master[i] -> Slots a - b`.
    Slots(usize, u16, u16),
    /// `Adding replica <replica> to <master>` (node indexes).
    Replica(usize, usize),
    ExtraReplicas,
}

/// Nodes in round-robin over their hosts, hosts in order of first appearance,
/// so the first masters picked sit on different hosts where possible.
pub(crate) fn interleave(hosts: &[&[u8]]) -> Vec<usize> {
    let mut groups: Vec<(&[u8], Vec<usize>)> = Vec::new();
    for (i, host) in hosts.iter().enumerate() {
        match groups.iter_mut().find(|(h, _)| h == host) {
            Some((_, members)) => members.push(i),
            None => groups.push((host, vec![i])),
        }
    }
    let longest = groups.iter().map(|(_, m)| m.len()).max().unwrap_or(0);
    (0..longest)
        .flat_map(|row| groups.iter().filter_map(move |(_, m)| m.get(row).copied()))
        .collect()
}

/// The inclusive slot range of master `i` of `masters`.
pub(crate) fn slot_range(i: usize, masters: usize) -> (u16, u16) {
    let per = SLOTS as f64 / masters as f64;
    let last = |k: usize| ((k + 1) as f64 * per - 1.0).round() as u16;
    let first = if i == 0 { 0 } else { last(i - 1) + 1 };
    let end = if i + 1 == masters { (SLOTS - 1) as u16 } else { last(i) };
    (first, end)
}

/// The whole plan for `hosts` (one per node) with `replicas` per master.
pub(crate) fn plan(hosts: &[&[u8]], replicas: usize) -> (Vec<Planned>, Vec<Note>) {
    let order = interleave(hosts);
    let masters_count = hosts.len() / (replicas + 1);
    let (masters, rest) = order.split_at(masters_count.min(order.len()));
    let mut notes = Vec::new();
    let mut planned: Vec<Planned> = Vec::new();
    for (i, &m) in masters.iter().enumerate() {
        let (a, b) = slot_range(i, masters_count);
        notes.push(Note::Slots(i, a, b));
        planned.push(Planned { node: m, slots: Some((a, b)), replicates: None });
    }
    // The spare nodes are offered starting from the second one.
    let mut spare: Vec<usize> = rest.iter().skip(1).chain(rest.first()).copied().collect();
    let mut assign = |master: usize, spare: &mut Vec<usize>, notes: &mut Vec<Note>| {
        let pick = spare.iter().position(|&n| hosts[n] != hosts[master]).unwrap_or(0);
        let replica = spare.remove(pick);
        notes.push(Note::Replica(replica, master));
        planned.push(Planned { node: replica, slots: None, replicates: Some(master) });
    };
    for &m in masters {
        for _ in 0..replicas {
            if !spare.is_empty() {
                assign(m, &mut spare, &mut notes);
            }
        }
    }
    if !spare.is_empty() {
        notes.push(Note::ExtraReplicas);
        for &m in masters.iter().cycle().take(spare.len()) {
            assign(m, &mut spare, &mut notes);
        }
    }
    planned.sort_by_key(|p| p.node);
    (planned, notes)
}

/// Same-host pairs: a replica with its master weighs 10000, two replicas of
/// one master on one host weigh 1.
pub(crate) fn affinity_score(hosts: &[&[u8]], planned: &[Planned]) -> (usize, usize) {
    let (mut with_master, mut together) = (0, 0);
    for p in planned {
        let Some(m) = p.replicates else { continue };
        with_master += usize::from(hosts[p.node] == hosts[m]);
        together += planned
            .iter()
            .filter(|q| {
                q.node > p.node && q.replicates == Some(m) && hosts[q.node] == hosts[p.node]
            })
            .count();
    }
    (with_master, together)
}

/// Swap replicas between masters while a swap lowers the score. Swaps that
/// only move a replica without helping are not made, so an assignment that
/// cannot be improved is kept as announced.
pub(crate) fn optimize(hosts: &[&[u8]], planned: &mut [Planned]) {
    let score = |p: &[Planned]| {
        let (a, b) = affinity_score(hosts, p);
        a * 10000 + b
    };
    let replicas: Vec<usize> =
        (0..planned.len()).filter(|&i| planned[i].replicates.is_some()).collect();
    let mut best = score(planned);
    let mut improved = true;
    while improved && best > 0 {
        improved = false;
        for (x, &i) in replicas.iter().enumerate() {
            for &j in &replicas[x + 1..] {
                let (a, b) = (planned[i].replicates, planned[j].replicates);
                if a == b {
                    continue;
                }
                (planned[i].replicates, planned[j].replicates) = (b, a);
                let now = score(planned);
                if now < best {
                    best = now;
                    improved = true;
                } else {
                    (planned[i].replicates, planned[j].replicates) = (a, b);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Note, affinity_score, interleave, optimize, plan, slot_range};

    #[test]
    fn slots_are_split_as_evenly_as_rounding_allows() {
        let ranges = |n| (0..n).map(|i| slot_range(i, n)).collect::<Vec<_>>();
        assert_eq!(ranges(3), [(0, 5460), (5461, 10922), (10923, 16383)]);
        assert_eq!(
            ranges(5),
            [(0, 3276), (3277, 6553), (6554, 9829), (9830, 13106), (13107, 16383)]
        );
        assert_eq!(ranges(7)[2..4], [(4681, 7021), (7022, 9361)]);
    }

    fn replicas_of(hosts: &[&[u8]], replicas: usize) -> Vec<(usize, usize)> {
        plan(hosts, replicas)
            .1
            .into_iter()
            .filter_map(|n| if let Note::Replica(r, m) = n { Some((r, m)) } else { None })
            .collect()
    }

    #[test]
    fn replicas_are_offered_in_the_order_redis_cli_announces() {
        let one: &[&[u8]] = &[&b"a"[..]; 8];
        assert_eq!(replicas_of(&one[..6], 1), [(4, 0), (5, 1), (3, 2)]);
        assert_eq!(replicas_of(&one[..7], 1), [(4, 0), (5, 1), (6, 2), (3, 0)]);
        assert_eq!(replicas_of(one, 1), [(5, 0), (6, 1), (7, 2), (4, 3)]);
        let (l, i): (&[u8], &[u8]) = (b"localhost", b"127.0.0.1");
        assert_eq!(interleave(&[l, l, l, i, i, i]), [0, 3, 1, 4, 2, 5]);
        assert_eq!(replicas_of(&[l, l, l, i, i, i], 1), [(5, 0), (2, 3), (4, 1)]);
        assert_eq!(replicas_of(&[l, i, l, i, l, i, i], 1), [(5, 0), (4, 1), (6, 2), (3, 0)]);
    }

    #[test]
    fn optimizing_swaps_only_when_it_helps() {
        let (a, b): (&[u8], &[u8]) = (b"a", b"b");
        let hosts = [a, b, a, b];
        let (mut planned, _) = plan(&hosts, 1);
        assert_eq!(affinity_score(&hosts, &planned), (0, 0));
        planned[2].replicates = Some(0);
        planned[3].replicates = Some(1);
        optimize(&hosts, &mut planned);
        assert_eq!(affinity_score(&hosts, &planned), (0, 0));
        let same: &[&[u8]] = &[a; 6];
        let (mut planned, _) = plan(same, 1);
        let before = planned.clone();
        optimize(same, &mut planned);
        assert_eq!(planned, before);
    }
}
