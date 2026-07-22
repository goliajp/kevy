//! The internal `MATCH.SCORE` (pass 2) argv parser, split from
//! `args.rs` for the 500-LOC house rule. A `#[path]` child of `args`,
//! so it shares the clause helpers.

use super::MatchArgs;

/// Parse the internal pass-2 argv
/// `[MATCH.SCORE, name, text, LIMIT=<n>, <gstats>, (clause…)?]` into the
/// same [`MatchArgs`] the user's own MATCH parsed into.
///
/// Only the head differs between the passes — pass 2 pins name, text and
/// limit positionally and carries the global-stats blob at index 4 (which
/// [`super::super::wire::decode_gstats_arg`] reads separately). The
/// trailing clauses are the ones the user wrote, so they go through the
/// *same* `apply_clause`: pass 1 and pass 2 cannot drift on what a clause
/// means, and a clause added to MATCH reaches the second pass by
/// construction. Shared by the per-shard op and the origin merge so their
/// view of the clauses can never drift either.
pub(crate) fn parse_match_score(argv: &[Vec<u8>]) -> Option<MatchArgs> {
    let mut a = MatchArgs {
        name: argv.get(1)?.clone(),
        text: argv.get(2)?.clone(),
        limit: std::str::from_utf8(argv.get(3)?).ok()?.strip_prefix("LIMIT=")?.parse().ok()?,
        fields: Vec::new(),
        highlight: None,
        typo: 0,
        offset: 0,
        scope: Vec::new(),
        filters: Vec::new(),
        sort: None,
        distinct: None,
    };
    let mut i = 5;
    while i < argv.len() {
        i = super::apply_clause(argv, i, &mut a)?;
    }
    a.limit = a.limit.clamp(1, 1000);
    a.offset = a.offset.min(10_000);
    Some(a)
}
