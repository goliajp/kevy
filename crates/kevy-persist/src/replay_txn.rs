//! Transaction-bracket recognition for AOF replay. Split from
//! `replay.rs` for the 500-LOC house rule.
//!
//! The writer brackets a transaction with marker records
//! (`Aof::TXN_BEGIN` / `TXN_COMMIT`); the walk in `replay.rs` holds
//! every frame after a begin and applies the batch only on the matching
//! commit, discarding it if the log ends first. That is what makes an
//! `atomic()` block all-or-nothing regardless of size — see
//! `aof_txn.rs` for why group commit alone is not enough.

use kevy_resp::Argv;

pub(crate) enum TxnMarker {
    Begin,
    Commit,
}

/// Is this record one of the transaction brackets rather than a command?
pub(crate) fn txn_marker(args: &Argv) -> Option<TxnMarker> {
    if args.len() != 1 {
        return None;
    }
    let v = &args[0];
    if v == crate::Aof::TXN_BEGIN {
        Some(TxnMarker::Begin)
    } else if v == crate::Aof::TXN_COMMIT {
        Some(TxnMarker::Commit)
    } else {
        None
    }
}

