//! Tests for `ops_table`, kept apart to hold the table file under 500 lines.
use super::*;

#[test]
fn no_duplicate_names() {
    let mut seen = std::collections::HashSet::new();
    for o in OP_TABLE {
        assert!(seen.insert(o.name), "duplicate OP_TABLE row: {}", o.name);
    }
}

#[test]
fn growing_implies_write_and_wake_implies_write() {
    for o in OP_TABLE {
        if o.growing {
            assert!(o.write, "{}: growing but not write", o.name);
        }
        if o.wake_idx.is_some() {
            assert!(o.write, "{}: wakes waiters but not write", o.name);
        }
    }
}

#[test]
fn known_gaps_reference_real_ops_and_are_actual_holes() {
    for (name, flag, _) in KNOWN_GAPS {
        let s = spec(name).unwrap_or_else(|| panic!("gap entry for unknown op {name}"));
        assert_eq!(
            s.surfaces & flag,
            0,
            "{name}: KNOWN_GAPS says surface {flag:#b} is missing, but the table has the bit set — \
             the gap was closed; remove the ledger entry"
        );
    }
}

#[test]
fn every_logged_verb_is_replayable() {
    // The no-silent-data-loss invariant: an op present on any embedded write
    // surface (facade/pipe/atomic) that is a write MUST have a
    // replay arm — unless it is explicitly ledgered.
    for o in OP_TABLE {
        let on_embedded_write =
            o.write && o.surfaces & (surface::ESTORE | surface::PIPE | surface::ATOMIC) != 0;
        if !on_embedded_write {
            continue;
        }
        let replayable = o.surfaces & surface::REPLAY != 0;
        let ledgered = KNOWN_GAPS.iter().any(|(n, f, _)| n == &o.name && f & surface::REPLAY != 0);
        // Ops whose AOF form is a DIFFERENT verb (documented effect
        // logging): BITOP and COPY log the SET of the result, and
        // the algebra stores log DEL + plain ZADD/SADD.
        let logs_as_other_verb = matches!(
            o.name,
            "BITOP"
                | "COPY"
                | "ZINTERSTORE"
                | "ZUNIONSTORE"
                | "ZDIFFSTORE"
                | "SINTERSTORE"
                | "SUNIONSTORE"
                | "SDIFFSTORE"
        );
        assert!(
            replayable || ledgered || logs_as_other_verb,
            "{}: embedded write surface without a replay arm and not ledgered — \
             this is the silent-data-loss-on-reopen class",
            o.name
        );
    }
}
