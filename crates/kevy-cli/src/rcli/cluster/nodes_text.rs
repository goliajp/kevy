//! One line of `CLUSTER NODES`, read into its fields.
//!
//! `<id> <ip:port@bus[,hostname]> <flags> <master|-> <ping-sent> <pong-recv>
//! <config-epoch> <link-state> <slot> <slot> ...`, where a slot is `n`,
//! `a-b`, `[n->-<id>]` (migrating to) or `[n-<-<id>]` (importing from).

use super::slots::SlotSet;

/// The flags this client acts on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    pub(crate) myself: bool,
    pub(crate) master: bool,
    pub(crate) replica: bool,
    pub(crate) fail: bool,
    pub(crate) pfail: bool,
    pub(crate) handshake: bool,
    pub(crate) noaddr: bool,
}

/// One node as some node sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) id: Vec<u8>,
    /// Empty when the node does not know its own address yet.
    pub(crate) host: Vec<u8>,
    pub(crate) port: i32,
    pub(crate) bus: i32,
    pub(crate) flags: Flags,
    pub(crate) master: Option<Vec<u8>>,
    pub(crate) epoch: u64,
    pub(crate) connected: bool,
    pub(crate) slots: SlotSet,
    /// `(slot, node id)`, in the order listed.
    pub(crate) migrating: Vec<(u16, Vec<u8>)>,
    pub(crate) importing: Vec<(u16, Vec<u8>)>,
}

/// Every well-formed line of a `CLUSTER NODES` reply, in order.
pub(crate) fn parse(text: &[u8]) -> Vec<Record> {
    text.split(|&b| b == b'\n').filter_map(line).collect()
}

fn line(line: &[u8]) -> Option<Record> {
    let fields: Vec<&[u8]> =
        line.split(|&b| b == b' ').filter(|f| !f.is_empty() && *f != b"\r").collect();
    let [id, addr, flags, master, _, _, epoch, link, slots @ ..] = fields.as_slice() else {
        return None;
    };
    let (host, port, bus) = address(addr)?;
    let mut record = Record {
        id: id.to_vec(),
        host,
        port,
        bus,
        flags: flag_set(flags),
        master: (*master != b"-").then(|| master.to_vec()),
        epoch: number(epoch)?,
        connected: *link == b"connected",
        ..Record::default()
    };
    for slot in slots {
        add_slot(&mut record, slot.strip_suffix(b"\r").unwrap_or(slot))?;
    }
    Some(record)
}

/// `ip:port@bus[,hostname]`; the port follows the last colon before `@`.
fn address(field: &[u8]) -> Option<(Vec<u8>, i32, i32)> {
    let field = field.split(|&b| b == b',').next()?;
    let (hostport, bus) = match field.iter().position(|&b| b == b'@') {
        Some(at) => (&field[..at], number(&field[at + 1..])?),
        None => (field, 0),
    };
    let colon = hostport.iter().rposition(|&b| b == b':')?;
    Some((hostport[..colon].to_vec(), number(&hostport[colon + 1..])?, bus))
}

fn flag_set(field: &[u8]) -> Flags {
    let mut f = Flags::default();
    for flag in field.split(|&b| b == b',') {
        match flag {
            b"myself" => f.myself = true,
            b"master" => f.master = true,
            b"slave" => f.replica = true,
            b"fail" => f.fail = true,
            b"fail?" => f.pfail = true,
            b"handshake" => f.handshake = true,
            b"noaddr" => f.noaddr = true,
            _ => {}
        }
    }
    f
}

fn add_slot(record: &mut Record, slot: &[u8]) -> Option<()> {
    if let Some(open) = slot.strip_prefix(b"[").and_then(|s| s.strip_suffix(b"]")) {
        let dash = open.iter().position(|&b| b == b'-')?;
        let n = number(&open[..dash])?;
        match &open[dash..] {
            [b'-', b'>', b'-', id @ ..] => record.migrating.push((n, id.to_vec())),
            [b'-', b'<', b'-', id @ ..] => record.importing.push((n, id.to_vec())),
            _ => return None,
        }
        return Some(());
    }
    let (first, last) = match slot.iter().position(|&b| b == b'-') {
        Some(dash) => (number(&slot[..dash])?, number(&slot[dash + 1..])?),
        None => (number(slot)?, number(slot)?),
    };
    for s in first..=last {
        record.slots.insert(s);
    }
    Some(())
}

fn number<T: std::str::FromStr>(text: &[u8]) -> Option<T> {
    std::str::from_utf8(text).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_nodes_reply_reads_into_records() {
        let text =
            b"aa 127.0.0.1:17000@27000 myself,master - 0 0 1 connected 0-2 5 [5->-bb] [7-<-cc]\n\
            bb ::1:17001@27001,host slave aa 0 1789621540581 4 disconnected\n\
            cc :17002@27002 master,fail?,noaddr - 0 0 0 connected\n\
            broken line\n";
        let records = parse(text);
        assert_eq!(records.len(), 3);
        let a = &records[0];
        assert_eq!(
            (a.id.as_slice(), a.host.as_slice(), a.port, a.bus),
            (&b"aa"[..], &b"127.0.0.1"[..], 17000, 27000)
        );
        assert!(a.flags.myself && a.flags.master && a.connected && a.master.is_none());
        assert_eq!((a.slots.count(), a.epoch), (4, 1));
        assert_eq!(a.migrating, vec![(5, b"bb".to_vec())]);
        assert_eq!(a.importing, vec![(7, b"cc".to_vec())]);
        let b = &records[1];
        assert_eq!(
            (b.host.as_slice(), b.port, b.master.as_deref()),
            (&b"::1"[..], 17001, Some(&b"aa"[..]))
        );
        assert!(b.flags.replica && !b.connected);
        let c = &records[2];
        assert!(c.host.is_empty() && c.flags.pfail && c.flags.noaddr && !c.flags.fail);
    }
}
