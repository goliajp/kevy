//! Where a command's keys are, as Redis describes them in `COMMAND INFO`
//! (the rows in [`crate::command_specs`], taken from the pinned Redis), and
//! `COMMAND GETKEYS` / `GETKEYSANDFLAGS` on top of them.
//!
//! Keys are found as Redis finds them: by the key specs, except for the
//! commands whose specs Redis marks incomplete or whose flags depend on
//! the arguments, which have rules of their own; and a command counted by
//! a `numkeys` argument falls back, when its spec cannot read the count,
//! to a rule that reads it leniently and reports no flags.

/// Where a key spec's search starts.
pub(crate) enum Begin {
    /// At this argument.
    Index(i32),
    /// Just after this keyword, searched from `startfrom` (backwards when
    /// negative).
    Keyword(&'static str, i32),
    Unknown,
}

/// How a key spec's keys run from where the search started.
pub(crate) enum Find {
    /// `(lastkey, keystep, limit)`.
    Range(i32, i32, i32),
    /// `(keynumidx, firstkey, keystep)`.
    KeyNum(i32, i32, i32),
    Unknown,
}

pub(crate) struct KeySpec {
    pub(crate) notes: &'static str,
    pub(crate) flags: &'static [&'static str],
    pub(crate) begin: Begin,
    pub(crate) find: Find,
}

/// One `COMMAND INFO` row.
pub(crate) struct CmdSpec {
    pub(crate) name: &'static str,
    pub(crate) arity: i32,
    pub(crate) flags: &'static [&'static str],
    pub(crate) first: i32,
    pub(crate) last: i32,
    pub(crate) step: i32,
    pub(crate) acl: &'static [&'static str],
    pub(crate) tips: &'static [&'static str],
    pub(crate) keys: &'static [KeySpec],
    pub(crate) subs: &'static [CmdSpec],
}

#[allow(clippy::too_many_arguments)]
pub(crate) const fn c(
    name: &'static str,
    arity: i32,
    flags: &'static [&'static str],
    first: i32,
    last: i32,
    step: i32,
    acl: &'static [&'static str],
    tips: &'static [&'static str],
    keys: &'static [KeySpec],
    subs: &'static [CmdSpec],
) -> CmdSpec {
    CmdSpec { name, arity, flags, first, last, step, acl, tips, keys, subs }
}

pub(crate) const fn k(
    notes: &'static str,
    flags: &'static [&'static str],
    begin: Begin,
    find: Find,
) -> KeySpec {
    KeySpec { notes, flags, begin, find }
}

/// The row of a command Redis also has, by name in any case.
pub(crate) fn spec(name: &[u8]) -> Option<&'static CmdSpec> {
    let specs = crate::command_specs::SPECS;
    let lower = name.to_ascii_lowercase();
    specs.binary_search_by(|s| s.name.as_bytes().cmp(&lower)).ok().map(|i| &specs[i])
}

/// A key found in an argv: its position and its flags.
pub(crate) type Key = (usize, &'static [&'static str]);

const RO: &[&str] = &["RO", "access"];
const OW: &[&str] = &["OW", "update"];
const RW_UPDATE: &[&str] = &["RW", "update"];
const RW_ACCESS: &[&str] = &["RW", "access", "update"];
const NONE: &[&str] = &[];

fn is(arg: &[u8], word: &str) -> bool {
    arg.eq_ignore_ascii_case(word.as_bytes())
}

/// The keys of `argv` by the command's specs; `None` when a spec cannot
/// be read against it, which makes the whole search fail.
fn by_specs(cmd: &CmdSpec, argv: &[&[u8]]) -> Option<Vec<Key>> {
    let argc = argv.len() as i64;
    let mut keys = Vec::new();
    for spec in cmd.keys {
        if spec.flags.contains(&"not_key") {
            continue;
        }
        let first = match spec.begin {
            Begin::Index(pos) => i64::from(pos),
            Begin::Keyword(word, from) => match after_keyword(argv, word, from) {
                Some(first) => first,
                None => continue,
            },
            Begin::Unknown => return None,
        };
        let (first, last, step) = span(&spec.find, argv, first)?;
        // a run to the end that holds no key: an optional list left empty
        if matches!(spec.find, Find::Range(lastkey, ..) if lastkey < 0) && last == first - 1 {
            continue;
        }
        if last >= argc || last < first || first >= argc {
            return None;
        }
        let mut i = first;
        while i <= last {
            keys.push((i as usize, spec.flags));
            i += step;
        }
    }
    Some(keys)
}

/// The argument after `word`, searched from `from` towards the end (or,
/// when negative, from the end towards the start), the bound excluded.
fn after_keyword(argv: &[&[u8]], word: &str, from: i32) -> Option<i64> {
    let argc = argv.len() as i64;
    let from = i64::from(from);
    let (start, end) = if from > 0 { (from, argc - 1) } else { (argc + from, 1) };
    let mut i = start;
    while i != end {
        if i >= argc || i < 1 {
            return None;
        }
        if is(argv[i as usize], word) {
            return Some(i + 1);
        }
        i += if start <= end { 1 } else { -1 };
    }
    None
}

/// `(first, last, step)` of a spec's keys from `first`.
fn span(find: &Find, argv: &[&[u8]], first: i64) -> Option<(i64, i64, i64)> {
    let argc = argv.len() as i64;
    match *find {
        Find::Range(lastkey, step, limit) => {
            let (lastkey, limit) = (i64::from(lastkey), i64::from(limit));
            let last = if lastkey >= 0 {
                first + lastkey
            } else if limit == 0 {
                argc + lastkey
            } else {
                first + ((argc - first) / limit + lastkey)
            };
            Some((first, last, i64::from(step)))
        }
        Find::KeyNum(idx, firstkey, step) => {
            let idx = i64::from(idx);
            if idx >= argc - first {
                return None;
            }
            let n = string2ll(argv[(first + idx) as usize])?;
            if n < 0 {
                return None;
            }
            let first = first + i64::from(firstkey);
            Some((first, first + n - 1, i64::from(step)))
        }
        Find::Unknown => None,
    }
}

/// An integer as Redis reads one strictly: no sign but `-`, no leading
/// zero, nothing else around it.
fn string2ll(s: &[u8]) -> Option<i64> {
    if s == b"0" {
        return Some(0);
    }
    let (neg, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, s),
    };
    if !matches!(digits.first(), Some(b'1'..=b'9')) || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v: i64 = 0;
    for &d in digits {
        let d = i64::from(d - b'0');
        v = v.checked_mul(10)?.checked_add(if neg { -d } else { d })?;
    }
    Some(v)
}

/// The commands whose keys Redis finds by a rule of its own: their specs
/// are incomplete, or their flags depend on the arguments.
fn own_rule(name: &str, argv: &[&[u8]]) -> Option<Vec<Key>> {
    Some(match name {
        "set" => vec![(1, set_flags(argv))],
        "bitfield" => vec![(1, bitfield_flags(argv))],
        "sort" => sort_keys(argv),
        "sort_ro" => vec![(1, RO)],
        "georadius" | "georadiusbymember" => {
            let mut keys = vec![(1, RO)];
            let mut i = 5;
            let mut store = None;
            while i < argv.len() {
                if (is(argv[i], "STORE") || is(argv[i], "STOREDIST")) && i + 1 < argv.len() {
                    store = Some(i + 1);
                    i += 1;
                }
                i += 1;
            }
            keys.extend(store.map(|at| (at, OW)));
            keys
        }
        "xread" | "xreadgroup" => xread_keys(argv),
        _ => return None,
    })
}

fn set_flags(argv: &[&[u8]]) -> &'static [&'static str] {
    let rest = argv.get(3..).unwrap_or_default();
    if rest.iter().any(|a| is(a, "GET")) {
        RW_ACCESS
    } else if rest.iter().any(|a| ["IFEQ", "IFNE", "IFDEQ", "IFDNE"].iter().any(|w| is(a, w))) {
        RW_UPDATE
    } else {
        OW
    }
}

/// Read-only when every operation is a complete GET (OVERFLOW aside).
fn bitfield_flags(argv: &[&[u8]]) -> &'static [&'static str] {
    let mut i = 2;
    while i < argv.len() {
        let left = argv.len() - i - 1;
        if is(argv[i], "GET") && left >= 2 {
            i += 3;
        } else if is(argv[i], "OVERFLOW") && left >= 1 {
            i += 2;
        } else {
            return RW_ACCESS;
        }
    }
    RO
}

/// The sorted key, and the last `STORE` destination; `LIMIT` takes two
/// values and `GET` / `BY` one, so none of those is taken for it.
fn sort_keys(argv: &[&[u8]]) -> Vec<Key> {
    let mut store = None;
    let mut i = 2;
    while i < argv.len() {
        if is(argv[i], "LIMIT") {
            i += 2;
        } else if is(argv[i], "GET") || is(argv[i], "BY") {
            i += 1;
        } else if is(argv[i], "STORE") && i + 1 < argv.len() {
            store = Some(i + 1);
        }
        i += 1;
    }
    let mut keys = vec![(1, RO)];
    keys.extend(store.map(|at| (at, OW)));
    keys
}

/// The streams after `STREAMS`, half of what follows it (the other half
/// are ids); options before it are stepped over by their arity.
fn xread_keys(argv: &[&[u8]]) -> Vec<Key> {
    let mut i = 1;
    let mut streams = None;
    while i < argv.len() {
        let a = argv[i];
        if is(a, "BLOCK") || is(a, "COUNT") || is(a, "CLAIM") {
            i += 1;
        } else if is(a, "GROUP") {
            i += 2;
        } else if is(a, "STREAMS") {
            streams = Some(i);
            break;
        } else if !is(a, "NOACK") {
            break;
        }
        i += 1;
    }
    let Some(at) = streams else { return Vec::new() };
    let n = argv.len() - at - 1;
    if n == 0 || !n.is_multiple_of(2) {
        return Vec::new();
    }
    (at + 1..at + 1 + n / 2).map(|i| (i, RO)).collect()
}

/// The rule a `numkeys` command falls back to when its spec cannot read
/// the count: the count read as C's `atoi` reads it, and no flags.
fn numkeys_rule(name: &str, argv: &[&[u8]]) -> Option<Vec<Key>> {
    // (where the count is, where the keys start, a destination before them)
    let (count_at, first, dest) = match name {
        "zunionstore" | "zinterstore" | "zdiffstore" => (2, 3, true),
        "zunion" | "zinter" | "zdiff" | "zintercard" | "sintercard" | "lmpop" | "zmpop" => {
            (1, 2, false)
        }
        "blmpop" | "bzmpop" => (2, 3, false),
        "eval" | "evalsha" => (2, 3, false),
        _ => return None,
    };
    let n = i64::from(atoi(argv.get(count_at)?));
    if n < 1 || n > argv.len() as i64 - first as i64 {
        return Some(Vec::new());
    }
    let mut keys: Vec<Key> = (first..first + n as usize).map(|i| (i, NONE)).collect();
    if dest {
        keys.push((1, NONE));
    }
    Some(keys)
}

/// C's `atoi`, as glibc gives it: leading blanks, a sign, digits up to the
/// first other byte; the `long` it reads cut to an `int`.
fn atoi(s: &[u8]) -> i32 {
    let s = s.trim_ascii_start();
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for &d in digits.iter().take_while(|d| d.is_ascii_digit()) {
        v = v.saturating_mul(10).saturating_add(i64::from(d - b'0'));
    }
    (if neg { v.saturating_neg() } else { v }) as i32
}

/// The keys of `argv` (the command and its arguments) for `cmd`, as
/// Redis finds them.
pub(crate) fn keys_of(cmd: &CmdSpec, argv: &[&[u8]]) -> Vec<Key> {
    let name = cmd.name;
    let irregular = cmd.keys.iter().any(|s| {
        s.flags.iter().any(|f| *f == "incomplete" || *f == "variable_flags")
            || matches!(s.begin, Begin::Unknown)
    });
    if irregular && let Some(keys) = own_rule(name, argv) {
        return keys;
    }
    match by_specs(cmd, argv) {
        Some(keys) => keys,
        None => numkeys_rule(name, argv).unwrap_or_default(),
    }
}
