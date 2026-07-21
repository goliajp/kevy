//! IDX.* read surface: the extension fan-out halves
//! (per-shard op + origin reduce) and the query grammar. Split from
//! [`crate::cmd_index`] under the 500-LOC house rule into submodules
//! ([`args`] grammar, [`ops`] ranked/agg/compose ops,
//! [`query`] scalar query + admin, [`wire`] chunk/cursor encoding).

mod args;
mod ops;
mod query;
mod wire;

pub(crate) use args::{
    ComposeQuery, FilterShape, HybridArgs, KnnArgs, MatchArgs, Query, parse_groups_args,
    parse_match_score,
};
pub(crate) use wire::{decode_value, decode_view_cursor, encode_value, hex};

use kevy_store::Store;

use crate::state::Ctx;

/// One hit's hydrated field values (None = field absent).
pub(crate) type Hydrated = Vec<Option<Vec<u8>>>;

/// One field's highlight: its name and the `(start, end)` match spans.
pub(crate) type FieldSpans = (Vec<u8>, Vec<(u32, u32)>);
/// One hit's highlight: [`FieldSpans`] per matched field. Shared across
/// the per-shard op, the wire codec and the reduce.
pub(crate) type HitSpans = Vec<FieldSpans>;

// ---------- extension fan-out (reads) ----------

pub(crate) const ST_OK: u8 = 0;
pub(crate) const ST_BUILDING: u8 = 1;
pub(crate) const ST_NOINDEX: u8 = 2;
pub(crate) const ST_BADARGS: u8 = 3;
pub(crate) const ST_OVERBUDGET: u8 = 4;
/// A clause of the terminal MATCH surface that parses but is not built
/// yet. Distinct from ST_BADARGS on purpose: "you wrote it wrong" and
/// "this is coming" are different answers, and collapsing them sends
/// people hunting for a typo in correct syntax. The chunk carries the
/// clause name after the status byte.
pub(crate) const ST_NOTYET: u8 = 5;
/// A clause the parser accepted but this index cannot answer — `IN`
/// naming an undeclared field, `FILTER` naming an unstored one, a bound
/// that is not of the declared type. The chunk carries the whole
/// explanation, because only the shard holding the spec knows what IS
/// declared, and "bad arguments" would send the caller hunting for a
/// typo in correct syntax.
pub(crate) const ST_CLAUSE: u8 = 6;

/// Per-shard half: parse the IDX.* argv, run against this shard's
/// segment, emit a status-tagged chunk.
pub(crate) fn extension_op(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"IDX.LIST") {
        return query::op_list(ctx, store);
    }
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) {
        return ops::op_hybrid(ctx, store, argv);
    }
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"COMPOSE")) {
        return ops::op_compose(ctx, store, argv);
    }
    // IDX.EXPLAIN <name> <shape…> — the exact IDX.QUERY parse,
    // ZERO execution.
    if verb.eq_ignore_ascii_case(b"IDX.EXPLAIN") {
        return query::op_explain(ctx, store, argv);
    }
    // Pass 2 of MATCH (internal): MATCH.SCORE — score against the injected
    // global CorpusStats (global BM25, step 4b-server).
    if verb.eq_ignore_ascii_case(b"MATCH.SCORE") {
        return ops::op_match_score(ctx, store, argv);
    }
    // IDX.QUERY <name> MATCH <text> [LIMIT n] [FIELDS f…] (pass 1: stats)
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"MATCH")) {
        return ops::op_match(ctx, store, argv);
    }
    // IDX.QUERY <name> KNN <vec> [LIMIT k] [FIELDS f…]
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"KNN")) {
        return ops::op_knn(ctx, store, argv);
    }
    // IDX.QUERY <name> GROUP <g> | GROUPS [BY m] [LIMIT n]
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"GROUP") || a.eq_ignore_ascii_case(b"GROUPS")) {
        return ops::op_agg(ctx, store, argv);
    }
    // Phase 2 of GROUPS (internal): AGG.FETCH <name> <g…> — exact partials
    // for the candidate groups that survived phase-1 ranking.
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"AGG.FETCH")) {
        return ops::op_agg_fetch(ctx, store, argv);
    }
    // IDX.REBUILD <name> (ANN tombstone compaction)
    if argv
        .first()
        .is_some_and(|v| v.eq_ignore_ascii_case(b"IDX.REBUILD"))
    {
        return ops::op_rebuild(ctx, store, argv);
    }
    query::op_query(ctx, store, argv, verb)
}
