//! What a write is recorded as, beyond the argv it was run with.
//!
//! A write that moves a deadline by a relative amount (`EXPIRE k 100`,
//! `SET … EX 100`) would, replayed later, count its TTL from the time of
//! the replay. So the record of such a write is followed by a frame
//! naming the absolute deadline it set, which a replay applies last.

use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, now_unix_ms};

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

/// The frame that must follow the record of `args`, run against
/// `store` just now: the absolute deadline a relative-TTL write set.
/// `None` for every other write.
///
/// Key deadlines (`EXPIRE`, `PEXPIRE`, `SETEX`, `PSETEX`, `SET … EX|PX`,
/// `GETEX … EX|PX`) are followed by `PEXPIREAT`; field deadlines
/// (`HEXPIRE`, `HPEXPIRE`) by `HPEXPIREAT` with the same fields.
///
/// ```
/// let mut store = kevy_store::Store::new();
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec(), b"EX".to_vec(), b"60".to_vec()]);
/// kevy_verbs::exec(&mut store, b"SET", &set, &mut Vec::new());
/// let f = kevy_verbs::aof::ttl_followup(&mut store, &set).unwrap();
/// assert_eq!(&f[0], b"PEXPIREAT");
/// ```
pub fn ttl_followup<A: ArgvView + ?Sized>(store: &mut Store, args: &A) -> Option<Argv> {
    let verb = args.get(0)?;
    if verb.eq_ignore_ascii_case(b"HEXPIRE") || verb.eq_ignore_ascii_case(b"HPEXPIRE") {
        return field_deadline_frame(args);
    }
    if !relative_ttl(args) {
        return None;
    }
    deadline_frame(store, args.get(1)?)
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

/// `HPEXPIREAT key <unix-ms> …` for a relative `HEXPIRE` / `HPEXPIRE`,
/// the tail after the TTL copied as it was given.
fn field_deadline_frame<A: ArgvView + ?Sized>(args: &A) -> Option<Argv> {
    if args.len() < 6 {
        return None;
    }
    let raw = crate::args::arg_i64(&args[2])?;
    let ms = if args[0].eq_ignore_ascii_case(b"HEXPIRE") { raw.saturating_mul(1000) } else { raw };
    let abs = now_unix_ms().saturating_add_signed(ms);
    let mut f = Argv::with_capacity(args.len(), 0);
    f.push(b"HPEXPIREAT");
    f.push(&args[1]);
    f.push(abs.to_string().as_bytes());
    for i in 3..args.len() {
        f.push(&args[i]);
    }
    Some(f)
}
