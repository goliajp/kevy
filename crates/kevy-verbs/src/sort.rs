//! `SORT` / `SORT_RO` over the key's own elements: `LIMIT`, `ALPHA`,
//! `ASC` / `DESC`, `STORE`, `BY` with a pattern that names no other key
//! (`BY nosort`), and `GET #`. A `BY` or `GET` pattern that names other
//! keys is refused as Redis refuses it in cluster mode: on a sharded
//! keyspace those keys may live on any shard.

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::Store;

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, store_err, wrong_args};
use crate::{Effect, changed};

const BY_DENIED: &str = "ERR BY option of SORT denied in Cluster mode when keys formed by the pattern may be in different slots.";
const GET_DENIED: &str = "ERR GET option of SORT denied in Cluster mode when keys formed by the pattern may be in different slots.";
const NOT_DOUBLE: &str = "ERR One or more scores can't be converted into double";

#[derive(Default)]
struct Opts {
    desc: bool,
    alpha: bool,
    nosort: bool,
    limit: Option<(i64, i64)>,
    /// How many `GET #` were given: each element repeated that often.
    gets: usize,
    /// The argument index of the `STORE` destination.
    store: Option<usize>,
}

/// One command of this group; `None` = not in the group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let read_only = match cmd {
        b"SORT" => false,
        b"SORT_RO" => true,
        _ => return None,
    };
    if args.len() < 2 {
        wrong_args(out, if read_only { "sort_ro" } else { "sort" });
        return Some(Effect::Unchanged);
    }
    let opts = match parse(args, read_only) {
        Ok(o) => o,
        Err(e) => {
            encode_error(out, e);
            return Some(Effect::Unchanged);
        }
    };
    Some(run(store, args, opts, out))
}

/// Where `SORT`'s `STORE` destination is in `args`, when it has one and the
/// options parse.
///
/// ```
/// let argv = kevy_resp::Argv::from(
///     "SORT src LIMIT 0 5 STORE dst ALPHA".split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>(),
/// );
/// assert_eq!(kevy_verbs::sort::store_destination(&argv), Some(6));
/// ```
pub fn store_destination<A: ArgvView + ?Sized>(args: &A) -> Option<usize> {
    (args.len() >= 2).then(|| parse(args, false).ok()?.store).flatten()
}

fn parse<A: ArgvView + ?Sized>(args: &A, read_only: bool) -> Result<Opts, &'static str> {
    let mut o = Opts::default();
    let mut j = 2;
    while j < args.len() {
        let (tok, left) = (&args[j], args.len() - j - 1);
        let is = |w: &[u8]| tok.eq_ignore_ascii_case(w);
        if is(b"ASC") {
            o.desc = false;
        } else if is(b"DESC") {
            o.desc = true;
        } else if is(b"ALPHA") {
            o.alpha = true;
        } else if is(b"LIMIT") && left >= 2 {
            let offset = arg_i64(&args[j + 1]).ok_or(ERR_NOT_INT)?;
            let count = arg_i64(&args[j + 2]).ok_or(ERR_NOT_INT)?;
            o.limit = Some((offset, count));
            j += 2;
        } else if is(b"STORE") && left >= 1 && !read_only {
            o.store = Some(j + 1);
            j += 1;
        } else if is(b"BY") && left >= 1 {
            // a pattern without `*` names no key: the elements stay unsorted
            if args[j + 1].contains(&b'*') {
                return Err(BY_DENIED);
            }
            o.nosort = true;
            j += 1;
        } else if is(b"GET") && left >= 1 {
            if &args[j + 1] != b"#" {
                return Err(GET_DENIED);
            }
            o.gets += 1;
            j += 1;
        } else {
            return Err(ERR_SYNTAX);
        }
        j += 1;
    }
    Ok(o)
}

fn run<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    mut o: Opts,
    out: &mut Vec<u8>,
) -> Effect {
    // the elements are borrowed in place; only the order is built apart
    let mut items: Vec<&[u8]> = Vec::new();
    let kind = match store.each_element(&args[1], |e| items.push(e)) {
        Ok(kind) => kind,
        Err(e) => {
            store_err(out, e);
            return Effect::Unchanged;
        }
    };
    // a set's own order is not stable, so an unsorted stored result is
    // sorted by bytes instead
    if o.nosort && kind == "set" && o.store.is_some() {
        (o.nosort, o.alpha) = (false, true);
    }
    if !o.nosort && sort(&mut items, o.alpha).is_err() {
        encode_error(out, NOT_DOUBLE);
        return Effect::Unchanged;
    }
    if o.desc {
        items.reverse();
    }
    let items = &items[window(items.len(), o.limit)];
    let repeat = o.gets.max(1);
    let Some(at) = o.store else {
        encode_array_len(out, (items.len() * repeat) as i64);
        for it in items {
            (0..repeat).for_each(|_| encode_bulk(out, it));
        }
        return Effect::Read;
    };
    // the destination is written while the source is read: copied first
    let rows: Vec<Vec<u8>> =
        items.iter().flat_map(|it| core::iter::repeat_n(it.to_vec(), repeat)).collect();
    let dst = &args[at];
    let existed = store.del(&[dst]) > 0;
    if !rows.is_empty() {
        let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        store.rpush(dst, &refs).expect("a removed key takes a list");
    }
    encode_integer(out, rows.len() as i64);
    changed(existed || !rows.is_empty())
}

/// Ascending: by bytes for `ALPHA`, else by numeric value with the bytes
/// breaking ties. `Err` when an element is not a number.
fn sort(items: &mut [&[u8]], alpha: bool) -> Result<(), ()> {
    if alpha {
        items.sort_unstable();
        return Ok(());
    }
    let mut keyed: Vec<(f64, &[u8])> = items
        .iter()
        .map(|it| kevy_num::parse_exact(it).map(|v| (v, *it)).ok_or(()))
        .collect::<Result<_, _>>()?;
    // -0 and 0 compare equal, as C compares them; NaN never parses
    keyed
        .sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).expect("no NaN").then_with(|| a.1.cmp(b.1)));
    items.iter_mut().zip(keyed).for_each(|(slot, (_, it))| *slot = it);
    Ok(())
}

/// `LIMIT offset count` over `len` items: a negative offset is 0, a
/// negative count is "to the end".
fn window(len: usize, limit: Option<(i64, i64)>) -> core::ops::Range<usize> {
    let Some((offset, count)) = limit else { return 0..len };
    let start = (offset.max(0) as usize).min(len);
    let take = if count < 0 { len } else { count as usize };
    start..start.saturating_add(take).min(len)
}
