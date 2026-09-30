//! A consumer group's read counter, its consumers' last active times, and
//! the delivery counts too large for the stream record, carried in a
//! snapshot beside the stream they belong to.
//!
//! The stream record's group section predates the first two and holds a
//! delivery count in 32 bits, so each group that needs any of these
//! travels as one `OP_GROUP_READS` record after `OP_EOF` (and after the
//! aux frame, when there is one), where a reader from before the record
//! stops: `[key][group][known u8][entries_read u64 LE if known]
//! [n u32 LE][consumer, active_ms u64 LE]*[m u32 LE][ms u64 LE, seq u64 LE,
//! count u64 LE]*`. A reader from before it loads the groups with the
//! counter unknown, no consumer active and such counts at `u32::MAX`.

use std::io::{self, Read, Write};

use kevy_store::{Store, StreamData, StreamId, Value};

use crate::snapshot_fmt::{OP_GROUP_READS, read_bytes, read_u8, read_u32, read_u64, write_bytes};

/// One group's counter and active times.
pub(crate) struct GroupReads {
    key: Vec<u8>,
    group: Vec<u8>,
    entries_read: Option<u64>,
    active: Vec<(Vec<u8>, u64)>,
    /// Pending entries whose delivery count passes `u32::MAX`.
    wide: Vec<(StreamId, u64)>,
}

/// The records `value` at `key` needs: none unless it is a stream with a
/// group that has a counter or an active consumer.
pub(crate) fn collect(key: &[u8], value: &Value, into: &mut Vec<GroupReads>) {
    let Value::Stream(s) = value else { return };
    collect_stream(key, s, into);
}

fn collect_stream(key: &[u8], s: &StreamData, into: &mut Vec<GroupReads>) {
    for (name, g) in s.groups() {
        let active: Vec<(Vec<u8>, u64)> = g
            .consumers()
            .filter_map(|(c, cs)| cs.last_active_ms().map(|at| (c.to_vec(), at)))
            .collect();
        let wide: Vec<(StreamId, u64)> = g
            .pending_range(..)
            .filter(|(_, p)| p.delivery_count > u64::from(u32::MAX))
            .map(|(id, p)| (id, p.delivery_count))
            .collect();
        if g.entries_read().is_none() && active.is_empty() && wide.is_empty() {
            continue;
        }
        into.push(GroupReads {
            key: key.to_vec(),
            group: name.to_vec(),
            entries_read: g.entries_read(),
            active,
            wide,
        });
    }
}

pub(crate) fn write<W: Write>(w: &mut W, r: &GroupReads) -> io::Result<()> {
    w.write_all(&[OP_GROUP_READS])?;
    write_bytes(w, &r.key)?;
    write_bytes(w, &r.group)?;
    match r.entries_read {
        Some(n) => {
            w.write_all(&[1])?;
            w.write_all(&n.to_le_bytes())?;
        }
        None => w.write_all(&[0])?,
    }
    w.write_all(&(r.active.len() as u32).to_le_bytes())?;
    for (consumer, at) in &r.active {
        write_bytes(w, consumer)?;
        w.write_all(&at.to_le_bytes())?;
    }
    w.write_all(&(r.wide.len() as u32).to_le_bytes())?;
    for (id, count) in &r.wide {
        w.write_all(&id.ms.to_le_bytes())?;
        w.write_all(&id.seq.to_le_bytes())?;
        w.write_all(&count.to_le_bytes())?;
    }
    Ok(())
}

/// Read one record's body (the opcode already consumed) and apply it to
/// the group it names, when `keep` keeps the key.
pub(crate) fn read_and_apply<R: Read>(
    r: &mut R,
    store: &mut Store,
    keep: &impl Fn(&[u8]) -> bool,
) -> io::Result<()> {
    let key = read_bytes(r)?;
    let group = read_bytes(r)?;
    let entries_read = match read_u8(r)? {
        0 => None,
        _ => Some(read_u64(r)?),
    };
    let n = read_u32(r)? as usize;
    let mut active = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let consumer = read_bytes(r)?;
        active.push((consumer, read_u64(r)?));
    }
    let m = read_u32(r)? as usize;
    let mut wide = Vec::with_capacity(m.min(1024));
    for _ in 0..m {
        let id = StreamId::new(read_u64(r)?, read_u64(r)?);
        wide.push((id, read_u64(r)?));
    }
    if !keep(&key) {
        return Ok(());
    }
    let bad = |e: kevy_store::StoreError| io::Error::new(io::ErrorKind::InvalidData, e.as_wire());
    store.xgroup_set_entries_read(&key, &group, entries_read).map_err(bad)?;
    for (consumer, at) in active {
        store.xgroup_consumer_active(&key, &group, &consumer, Some(at)).map_err(bad)?;
    }
    for (id, count) in wide {
        store.xgroup_set_delivery_count(&key, &group, id, count).map_err(bad)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kevy_store::{AckMode, GroupCreateMode, MissingStream, ReadGroupId, XAddIdSpec};

    use super::*;

    fn stream_with_reads() -> Store {
        let mut s = Store::new();
        for ms in 1..=3 {
            let f = vec![(b"f".to_vec(), b"v".to_vec())];
            s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)
                .unwrap();
        }
        let at = GroupCreateMode::AtId(StreamId::MIN);
        for g in [&b"g"[..], b"idle", b"set"] {
            s.xgroup_create(b"s", g, at, MissingStream::Refuse).unwrap();
        }
        s.xreadgroup(b"s", b"g", b"c", ReadGroupId::New, Some(2), AckMode::Pending, 40).unwrap();
        s.xgroup_create_consumer(b"s", b"g", b"never", 50).unwrap();
        s.xgroup_set_entries_read(b"s", b"set", Some(9)).unwrap();
        s
    }

    /// `(group, entries_read, [(consumer, active)])`.
    type Reads = (Vec<u8>, Option<u64>, Vec<(Vec<u8>, Option<u64>)>);

    /// The reads of each group, by name.
    fn reads(s: &Store) -> Vec<Reads> {
        let mut groups = Vec::new();
        for name in [&b"g"[..], b"idle", b"set"] {
            let g = s.stream_group_peek(b"s", name).unwrap();
            let mut cs: Vec<_> =
                g.consumers().map(|(n, c)| (n.to_vec(), c.last_active_ms())).collect();
            cs.sort();
            groups.push((name.to_vec(), g.entries_read(), cs));
        }
        groups
    }

    #[test]
    fn the_counter_and_active_times_come_back_from_a_snapshot() {
        let s = stream_with_reads();
        let want = reads(&s);
        assert_eq!(want[0].1, Some(2));
        assert_eq!(want[0].2, [(b"c".to_vec(), Some(40)), (b"never".to_vec(), None)]);
        let mut image = Vec::new();
        crate::write_snapshot_to(&s, &mut image).unwrap();
        let mut back = Store::new();
        crate::load_snapshot_from(&mut back, image.as_slice()).unwrap();
        assert_eq!(reads(&back), want);
        // a key the loader does not keep takes no record either
        let mut none = Store::new();
        crate::load_snapshot_filtered(&mut none, image.as_slice(), |_| false).unwrap();
        assert_eq!(none.dbsize(), 0);
    }

    /// A reader that stops at `OP_EOF` loads every entry and group: the
    /// records follow what it reads.
    #[test]
    fn the_records_follow_the_end_of_the_keyspace() {
        let s = stream_with_reads();
        let mut image = Vec::new();
        crate::write_snapshot_to(&s, &mut image).unwrap();
        let mut records = Vec::new();
        crate::SnapshotSource::for_each_entry(&s, |k, v, _| collect(k, v, &mut records));
        assert_eq!(records.len(), 2, "g has a counter and an active consumer, set a counter");
        let mut tail = Vec::new();
        for r in &records {
            write(&mut tail, r).unwrap();
        }
        assert!(image.ends_with(&tail));
        assert_eq!(image[image.len() - tail.len() - 1], crate::snapshot_fmt::OP_EOF);
    }
}
