//! Argument grammars of the commands that span keys — the zset-algebra
//! `*STORE` forms, `ZINTERCARD`, `BITOP`, `COPY` — shared by the server,
//! which spreads them across shards, and the embedded engine, which runs
//! them under its own lock policy. Each parser answers where the keys
//! are, not copies of them, so a caller borrows or copies as it needs.
//! The caller has already checked the command's minimum arity.

use kevy_resp::{ArgvView, CmdError};
use kevy_store::{BitOp, ZAggregate};

/// A parsed `VERB dst numkeys key… [WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]`:
/// the destination is argument 1, the keys arguments `3..3 + numkeys`.
#[derive(Debug, Clone, PartialEq)]
pub struct ZStoreArgs {
    /// How many source keys follow the count.
    pub numkeys: usize,
    /// One weight per source key, when WEIGHTS was given.
    pub weights: Option<Vec<f64>>,
    /// How scores of one member combine across the sources.
    pub aggregate: ZAggregate,
}

/// `ZINTERSTORE` / `ZUNIONSTORE`, and `ZDIFFSTORE` when `diff_form` (it
/// takes neither WEIGHTS nor AGGREGATE).
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_verbs::multikey::parse_zstore;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// let z = parse_zstore(&a(&["ZUNIONSTORE", "d", "2", "a", "b", "WEIGHTS", "1", "2"]), false)?;
/// assert_eq!((z.numkeys, z.weights), (2, Some(vec![1.0, 2.0])));
/// assert!(parse_zstore(&a(&["ZDIFFSTORE", "d", "1", "a", "WEIGHTS", "1"]), true).is_err());
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
pub fn parse_zstore<A: ArgvView + ?Sized>(
    args: &A,
    diff_form: bool,
) -> Result<ZStoreArgs, CmdError> {
    if args.len() < 4 {
        return Err(CmdError::Wire("ERR wrong number of arguments"));
    }
    let numkeys = parse_numkeys(&args[2])?;
    if args.len() < 3 + numkeys {
        return Err(CmdError::Wire("ERR Number of keys can't be greater than number of args"));
    }
    let mut weights = None;
    let mut aggregate = ZAggregate::Sum;
    let mut i = 3 + numkeys;
    while i < args.len() {
        let a = &args[i];
        if !diff_form && a.eq_ignore_ascii_case(b"WEIGHTS") {
            if args.len() < i + 1 + numkeys {
                return Err(CmdError::Wire("ERR syntax error"));
            }
            let mut w = Vec::with_capacity(numkeys);
            for j in 0..numkeys {
                let v = std::str::from_utf8(&args[i + 1 + j])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or("ERR weight value is not a float")?;
                w.push(v);
            }
            weights = Some(w);
            i += 1 + numkeys;
        } else if !diff_form && a.eq_ignore_ascii_case(b"AGGREGATE") {
            aggregate = match args.get(i + 1).ok_or("ERR syntax error")? {
                m if m.eq_ignore_ascii_case(b"SUM") => ZAggregate::Sum,
                m if m.eq_ignore_ascii_case(b"MIN") => ZAggregate::Min,
                m if m.eq_ignore_ascii_case(b"MAX") => ZAggregate::Max,
                _ => return Err(CmdError::Wire("ERR syntax error")),
            };
            i += 2;
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
    }
    Ok(ZStoreArgs { numkeys, weights, aggregate })
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
