//! Argument grammars of the commands that span keys — the zset-algebra
//! `*STORE` forms, `ZINTERCARD`, `BITOP`, `COPY` — shared by the server,
//! which spreads them across shards, and the embedded engine, which runs
//! them under its own lock policy. Each parser answers where the keys
//! are, not copies of them, so a caller borrows or copies as it needs.
//! The caller has already checked the command's minimum arity.

use kevy_resp::{ArgvView, CmdError};
use kevy_store::{BitOp, ZAggregate};

/// A parsed zset combination: `[dst] numkeys key… [WEIGHTS w…]
/// [AGGREGATE SUM|MIN|MAX] [WITHSCORES]`. The `*STORE` forms put the
/// destination at argument 1 and the keys at `3..3 + numkeys`; the reply
/// forms (`ZINTER` / `ZUNION` / `ZDIFF`) the keys at `2..2 + numkeys`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ZStoreArgs {
    /// How many source keys follow the count.
    pub numkeys: usize,
    /// One weight per source key, when WEIGHTS was given.
    pub weights: Option<Vec<f64>>,
    /// How scores of one member combine across the sources.
    pub aggregate: ZAggregate,
    /// A reply form asked for the scores too.
    pub withscores: bool,
}

/// Which of the six combination commands is being parsed.
#[derive(Clone, Copy)]
struct Form {
    name: &'static str,
    store: bool,
    diff: bool,
}

/// `ZINTERSTORE` / `ZUNIONSTORE`.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_zstore;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// let z = parse_zstore(&a(&["ZUNIONSTORE", "d", "2", "a", "b", "WEIGHTS", "1", "2"]))?;
/// assert_eq!((z.numkeys, z.weights), (2, Some(vec![1.0, 2.0])));
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_zstore<A: ArgvView + ?Sized>(args: &A) -> Result<ZStoreArgs, CmdError> {
    let inter = args[0].eq_ignore_ascii_case(b"ZINTERSTORE");
    let name = if inter { "zinterstore" } else { "zunionstore" };
    combine(args, Form { name, store: true, diff: false })
}

/// `ZDIFFSTORE`: the same shape, but neither WEIGHTS nor AGGREGATE.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_zdiffstore;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// assert_eq!(parse_zdiffstore(&a(&["ZDIFFSTORE", "d", "2", "a", "b"]))?.numkeys, 2);
/// assert!(parse_zdiffstore(&a(&["ZDIFFSTORE", "d", "1", "a", "WEIGHTS", "1"])).is_err());
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_zdiffstore<A: ArgvView + ?Sized>(args: &A) -> Result<ZStoreArgs, CmdError> {
    combine(args, Form { name: "zdiffstore", store: true, diff: true })
}

/// `ZINTER` / `ZUNION numkeys key… [WEIGHTS w…] [AGGREGATE …] [WITHSCORES]`.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_zcombine;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// let z = parse_zcombine(&a(&["ZINTER", "2", "a", "b", "WITHSCORES"]))?;
/// assert_eq!((z.numkeys, z.withscores), (2, true));
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_zcombine<A: ArgvView + ?Sized>(args: &A) -> Result<ZStoreArgs, CmdError> {
    let name = if args[0].eq_ignore_ascii_case(b"ZINTER") { "zinter" } else { "zunion" };
    combine(args, Form { name, store: false, diff: false })
}

/// `ZDIFF numkeys key… [WITHSCORES]`.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_zdiff;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// assert!(parse_zdiff(&a(&["ZDIFF", "1", "a", "AGGREGATE", "MIN"])).is_err());
/// ```
pub fn parse_zdiff<A: ArgvView + ?Sized>(args: &A) -> Result<ZStoreArgs, CmdError> {
    combine(args, Form { name: "zdiff", store: false, diff: true })
}

fn combine<A: ArgvView + ?Sized>(args: &A, form: Form) -> Result<ZStoreArgs, CmdError> {
    let at = if form.store { 2 } else { 1 };
    let syntax = || CmdError::Wire("ERR syntax error");
    let numkeys = match args.get(at).map(crate::args::arg_i64) {
        Some(Some(n)) if n >= 1 => n as usize,
        Some(Some(_)) => return Err(CmdError::Wire(at_least_one(form.name))),
        _ => return Err(CmdError::Wire("ERR value is not an integer or out of range")),
    };
    if args.len() < at + 1 + numkeys {
        return Err(syntax());
    }
    let (mut weights, mut aggregate, mut withscores) = (None, ZAggregate::Sum, false);
    let mut i = at + 1 + numkeys;
    while i < args.len() {
        let a = &args[i];
        if !form.diff && a.eq_ignore_ascii_case(b"WEIGHTS") {
            if args.len() < i + 1 + numkeys {
                return Err(syntax());
            }
            let w: Option<Vec<f64>> = (0..numkeys)
                .map(|j| std::str::from_utf8(&args[i + 1 + j]).ok()?.parse::<f64>().ok())
                .collect();
            weights = Some(w.ok_or("ERR weight value is not a float")?);
            i += 1 + numkeys;
        } else if !form.diff && a.eq_ignore_ascii_case(b"AGGREGATE") {
            aggregate = match args.get(i + 1).ok_or_else(syntax)? {
                m if m.eq_ignore_ascii_case(b"SUM") => ZAggregate::Sum,
                m if m.eq_ignore_ascii_case(b"MIN") => ZAggregate::Min,
                m if m.eq_ignore_ascii_case(b"MAX") => ZAggregate::Max,
                _ => return Err(syntax()),
            };
            i += 2;
        } else if !form.store && a.eq_ignore_ascii_case(b"WITHSCORES") {
            withscores = true;
            i += 1;
        } else {
            return Err(syntax());
        }
    }
    Ok(ZStoreArgs { numkeys, weights, aggregate, withscores })
}

fn at_least_one(name: &str) -> &'static str {
    match name {
        "zinterstore" => "ERR at least 1 input key is needed for 'zinterstore' command",
        "zunionstore" => "ERR at least 1 input key is needed for 'zunionstore' command",
        "zdiffstore" => "ERR at least 1 input key is needed for 'zdiffstore' command",
        "zinter" => "ERR at least 1 input key is needed for 'zinter' command",
        "zunion" => "ERR at least 1 input key is needed for 'zunion' command",
        _ => "ERR at least 1 input key is needed for 'zdiff' command",
    }
}

/// `ZINTERCARD numkeys key… [LIMIT n]`: the key count (keys are arguments
/// `2..2 + numkeys`) and the limit, 0 for none.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_zintercard;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// assert_eq!(parse_zintercard(&a(&["ZINTERCARD", "2", "a", "b", "LIMIT", "5"]))?, (2, 5));
/// assert!(parse_zintercard(&a(&["ZINTERCARD", "0", "a"])).is_err());
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_zintercard<A: ArgvView + ?Sized>(args: &A) -> Result<(usize, usize), CmdError> {
    if args.len() < 3 {
        return Err(CmdError::Wire("ERR wrong number of arguments"));
    }
    let numkeys = parse_numkeys(&args[1])?;
    if args.len() < 2 + numkeys {
        return Err(CmdError::Wire("ERR Number of keys can't be greater than number of args"));
    }
    let mut limit = 0usize;
    let mut i = 2 + numkeys;
    while i < args.len() {
        if !args[i].eq_ignore_ascii_case(b"LIMIT") {
            return Err(CmdError::Wire("ERR syntax error"));
        }
        limit = args
            .get(i + 1)
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|s| s.parse().ok())
            .ok_or("ERR LIMIT can't be negative")?;
        i += 2;
    }
    Ok((numkeys, limit))
}

fn parse_numkeys(b: &[u8]) -> Result<usize, CmdError> {
    Ok(std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .ok_or("ERR numkeys should be greater than 0")?)
}

/// `BITOP <AND|OR|XOR|NOT> dst src [src …]`: the operation; the
/// destination is argument 2, the sources the rest.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_store::BitOp;
/// use kevy_verbs::multikey::parse_bitop;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// assert_eq!(parse_bitop(&a(&["BITOP", "xor", "d", "a", "b"]))?, BitOp::Xor);
/// assert!(parse_bitop(&a(&["BITOP", "NOT", "d", "a", "b"])).is_err(), "NOT takes one source");
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_bitop<A: ArgvView + ?Sized>(args: &A) -> Result<BitOp, CmdError> {
    if args.len() < 4 {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'bitop' command"));
    }
    let op = match args[1].to_ascii_uppercase().as_slice() {
        b"AND" => BitOp::And,
        b"OR" => BitOp::Or,
        b"XOR" => BitOp::Xor,
        b"NOT" => BitOp::Not,
        _ => return Err(CmdError::Wire("ERR syntax error")),
    };
    if op == BitOp::Not && args.len() != 4 {
        return Err(CmdError::Wire("ERR BITOP NOT must be called with a single source key."));
    }
    Ok(op)
}

/// `COPY src dst [REPLACE]`: whether to replace an existing destination.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_copy;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// assert_eq!(parse_copy(&a(&["COPY", "a", "b", "replace"]))?, true);
/// assert!(parse_copy(&a(&["COPY", "a", "a"])).is_err(), "a key onto itself");
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_copy<A: ArgvView + ?Sized>(args: &A) -> Result<bool, CmdError> {
    let replace = match args.len() {
        3 => false,
        4 if args[3].eq_ignore_ascii_case(b"REPLACE") => true,
        4 => return Err(CmdError::Wire("ERR syntax error")),
        _ => return Err(CmdError::Wire("ERR wrong number of arguments for 'copy' command")),
    };
    if args[1] == args[2] {
        return Err(CmdError::Wire("ERR source and destination objects are the same"));
    }
    Ok(replace)
}
