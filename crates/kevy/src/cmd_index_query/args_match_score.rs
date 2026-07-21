//! The internal `MATCH.SCORE` (pass 2) argv parser, split from
//! `args.rs` for the 500-LOC house rule. A `#[path]` child of `args`,
//! so it shares the clause helpers.

use super::{collect_clause, parse_typo};

/// Parsed pass-2 args: `(name, text, limit, fields, highlight, typo, offset)`.
pub(crate) type MatchScoreArgs =
    (Vec<u8>, Vec<u8>, usize, Vec<Vec<u8>>, Option<Vec<Vec<u8>>>, u32, usize);

/// Parse the internal pass-2 argv
/// `[MATCH.SCORE, name, text, LIMIT=<n>, <gstats>, (FIELDS f…)? (HIGHLIGHT h…)?]`
/// into `(name, text, limit, fields, highlight)`. The global-stats blob
/// at index 4 is decoded separately (per-shard:
/// [`super::wire::decode_gstats_arg`]); the reduce takes only the rest.
/// Shared by the per-shard op and the origin merge so their view of the
/// clauses can never drift.
pub(crate) fn parse_match_score(argv: &[Vec<u8>]) -> Option<MatchScoreArgs> {
    let name = argv.get(1)?.clone();
    let text = argv.get(2)?.clone();
    let limit: usize = std::str::from_utf8(argv.get(3)?)
        .ok()?
        .strip_prefix("LIMIT=")?
        .parse()
        .ok()?;
    // index 4 is the gstats blob; the optional clauses start at 5.
    let mut fields = Vec::new();
    let mut highlight = None;
    let mut typo = 0u32;
    let mut offset = 0usize;
    let mut i = 5;
    while i < argv.len() {
        let kw = &argv[i];
        if kw.eq_ignore_ascii_case(b"FIELDS") {
            let (fs, next) = collect_clause(argv, i + 1);
            if fs.is_empty() {
                return None;
            }
            fields = fs;
            i = next;
        } else if kw.eq_ignore_ascii_case(b"HIGHLIGHT") {
            let (hs, next) = collect_clause(argv, i + 1);
            highlight = Some(hs);
            i = next;
        } else if kw.eq_ignore_ascii_case(b"TYPO") {
            typo = parse_typo(argv.get(i + 1)?)?;
            i += 2;
        } else if kw.eq_ignore_ascii_case(b"OFFSET") {
            offset = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            i += 2;
        } else {
            return None;
        }
    }
    Some((name, text, limit.clamp(1, 1000), fields, highlight, typo, offset.min(10_000)))
}
