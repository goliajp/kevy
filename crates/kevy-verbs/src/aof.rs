//! What a write is recorded as, beyond the argv it was run with.
//!
//! A write that moves a deadline by a relative amount (`EXPIRE k 100`,
//! `SET … EX 100`) would, replayed later, count its TTL from the time of
//! the replay. So the record of such a write is followed by a frame
//! naming the absolute deadline it set, which a replay applies last.
//!
//! ```
//! use kevy_resp::Argv;
//! let mut store = kevy_store::Store::new();
//! let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec(), b"EX".to_vec(), b"100".to_vec()]);
//! let mut out = Vec::new();
//! kevy_verbs::exec(&mut store, b"SET", &set, &mut out);
//! let follow = kevy_verbs::aof::ttl_followup(&mut store, &set);
//! assert_eq!(&follow[0][0], b"PEXPIREAT", "a relative TTL gets a deadline frame");
//! ```

use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, now_unix_ms};

pub use crate::record::{Claim, deferred_frames, id_bytes};

/// The record of an `SPOP` that removed `popped`: `SREM key member…`.
/// Replaying `SPOP` itself would draw different members.
///
/// ```
/// let popped = vec![b"a".to_vec()];
/// let f = kevy_verbs::aof::spop_effect(b"s", &popped);
/// assert_eq!(f, vec![&b"SREM"[..], b"s", b"a"]);
/// ```
pub fn spop_effect<'a>(key: &'a [u8], popped: &'a [Vec<u8>]) -> Vec<&'a [u8]> {
    let mut frame: Vec<&[u8]> = Vec::with_capacity(2 + popped.len());
    frame.push(b"SREM");
    frame.push(key);
    frame.extend(popped.iter().map(Vec::as_slice));
    frame
}

/// `PEXPIREAT key <unix-ms>` for the deadline `key` has now, or `None`
/// when it has none (no key, or no TTL).
///
/// ```
/// let mut store = kevy_store::Store::new();
/// store.set(b"k", b"v".to_vec(), Some(std::time::Duration::from_secs(60)), false, false);
/// let f = kevy_verbs::aof::deadline_frame(&mut store, b"k").unwrap();
/// assert_eq!(&f[0], b"PEXPIREAT");
/// assert!(kevy_verbs::aof::deadline_frame(&mut store, b"missing").is_none());
/// ```
pub fn deadline_frame(store: &mut Store, key: &[u8]) -> Option<Argv> {
    let pttl = store.pttl(key);
    if pttl < 0 {
        return None;
    }
    let abs = now_unix_ms().saturating_add(pttl as u64);
    let mut f = Argv::with_capacity(3, 0);
    f.push(b"PEXPIREAT");
    f.push(key);
    f.push(abs.to_string().as_bytes());
    Some(f)
}

/// The frames that must follow the record of `args`, run against
/// `store` just now: the absolute deadlines a relative-TTL write set.
/// Empty for every other write.
///
/// Key deadlines (`EXPIRE`, `PEXPIRE`, `SETEX`, `PSETEX`, `SET … EX|PX`,
/// `GETEX … EX|PX`) are followed by `PEXPIREAT`. Field deadlines
/// (`HEXPIRE`, `HPEXPIRE`) are followed by `HPEXPIREAT … FIELDS`, one
/// frame per deadline the named fields hold now, with no condition: the
/// command's own record re-evaluates `NX|XX|GT|LT` on replay, and the
/// deadline frames then put every field back where it stood.
///
/// ```
/// let mut store = kevy_store::Store::new();
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec(), b"EX".to_vec(), b"60".to_vec()]);
/// kevy_verbs::exec(&mut store, b"SET", &set, &mut Vec::new());
/// let f = kevy_verbs::aof::ttl_followup(&mut store, &set);
/// assert_eq!(&f[0][0], b"PEXPIREAT");
/// ```
pub fn ttl_followup<A: ArgvView + ?Sized>(store: &mut Store, args: &A) -> Vec<Argv> {
    let Some(verb) = args.get(0) else { return Vec::new() };
    if verb.eq_ignore_ascii_case(b"HEXPIRE") || verb.eq_ignore_ascii_case(b"HPEXPIRE") {
        return field_deadline_frames(store, args);
    }
    if !relative_ttl(args) {
        return Vec::new();
    }
    args.get(1).and_then(|k| deadline_frame(store, k)).into_iter().collect()
}

/// Whether `args` moves a key's deadline by a relative amount.
fn relative_ttl<A: ArgvView + ?Sized>(args: &A) -> bool {
    if args.len() < 3 {
        return false;
    }
    let verb = &args[0];
    if verb.eq_ignore_ascii_case(b"EXPIRE")
        || verb.eq_ignore_ascii_case(b"PEXPIRE")
        || verb.eq_ignore_ascii_case(b"SETEX")
        || verb.eq_ignore_ascii_case(b"PSETEX")
    {
        return true;
    }
    let from = if verb.eq_ignore_ascii_case(b"SET") {
        3
    } else if verb.eq_ignore_ascii_case(b"GETEX") {
        2
    } else {
        return false;
    };
    (from..args.len())
        .any(|i| args[i].eq_ignore_ascii_case(b"EX") || args[i].eq_ignore_ascii_case(b"PX"))
}

/// `HPEXPIREAT key <unix-ms> FIELDS n field…` for each deadline the
/// fields named by a relative `HEXPIRE` / `HPEXPIRE` hold after it ran.
fn field_deadline_frames<A: ArgvView + ?Sized>(store: &mut Store, args: &A) -> Vec<Argv> {
    let Some(fields) = named_fields(args) else { return Vec::new() };
    let Ok(ttls) = store.hpttl(&args[1], &fields) else { return Vec::new() };
    let now = now_unix_ms();
    let mut by_deadline: Vec<(u64, Vec<&[u8]>)> = Vec::new();
    for (f, ttl) in fields.iter().zip(ttls) {
        let Ok(ms) = u64::try_from(ttl) else { continue };
        let at = now.saturating_add(ms);
        match by_deadline.iter_mut().find(|(d, _)| *d == at) {
            Some((_, group)) => group.push(f),
            None => by_deadline.push((at, vec![f])),
        }
    }
    by_deadline
        .into_iter()
        .map(|(at, group)| {
            let mut f = Argv::with_capacity(5 + group.len(), 0);
            f.push(b"HPEXPIREAT");
            f.push(&args[1]);
            f.push(at.to_string().as_bytes());
            f.push(b"FIELDS");
            f.push(group.len().to_string().as_bytes());
            for field in group {
                f.push(field);
            }
            f
        })
        .collect()
}

/// The fields after `FIELDS n`, as many as `n` says and the argv holds.
fn named_fields<A: ArgvView + ?Sized>(args: &A) -> Option<Vec<&[u8]>> {
    let at = (3..args.len()).find(|&i| args[i].eq_ignore_ascii_case(b"FIELDS"))?;
    let n = usize::try_from(crate::args::arg_i64(args.get(at + 1)?)?).ok()?;
    let first = at + 2;
    Some((first..args.len().min(first + n)).map(|i| &args[i]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&[u8]]) -> Argv {
        Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
    }

    // a field record the replay could not apply is worse than none: every
    // malformed or inapplicable shape produces no frame at all
    #[test]
    fn a_field_record_needs_fields_that_hold_a_deadline() {
        let mut store = Store::new();
        let mut out = Vec::new();
        crate::exec(&mut store, b"HSET", &argv(&[b"HSET", b"h", b"f", b"v", b"g", b"w"]), &mut out);
        crate::exec(&mut store, b"SET", &argv(&[b"SET", b"s", b"v"]), &mut out);
        for bad in [
            &[&b"HEXPIRE"[..], b"h", b"9"][..],
            &[b"HEXPIRE", b"h", b"9", b"NX"],
            &[b"HEXPIRE", b"h", b"9", b"FIELDS"],
            &[b"HEXPIRE", b"h", b"9", b"FIELDS", b"two", b"f"],
            &[b"HEXPIRE", b"h", b"9", b"FIELDS", b"-1", b"f"],
            &[b"HEXPIRE", b"s", b"9", b"FIELDS", b"1", b"f"],
            &[b"HEXPIRE", b"h", b"9", b"FIELDS", b"1", b"gone"],
            &[b"HEXPIRE", b"h", b"9", b"FIELDS", b"1", b"f"],
        ] {
            assert!(ttl_followup(&mut store, &argv(bad)).is_empty(), "{bad:?}");
        }
        // a count past the fields given is refused by the command itself
        let over = argv(&[b"HEXPIRE", b"h", b"60", b"FIELDS", b"9", b"f", b"g"]);
        crate::exec(&mut store, b"HEXPIRE", &over, &mut out);
        assert!(ttl_followup(&mut store, &over).is_empty());
        let set = argv(&[b"HEXPIRE", b"h", b"60", b"FIELDS", b"2", b"f", b"g"]);
        crate::exec(&mut store, b"HEXPIRE", &set, &mut out);
        let f = ttl_followup(&mut store, &set);
        assert_eq!(f.len(), 1, "one deadline, one frame");
        assert_eq!((&f[0][3], &f[0][4], f[0].len()), (&b"FIELDS"[..], &b"2"[..], 7));
    }
}
