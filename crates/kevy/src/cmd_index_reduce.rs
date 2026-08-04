//! IDX.* origin-side reduce: merge per-shard chunks into RESP
//! (split from [`crate::cmd_index_query`] under the 500-LOC rule
//! into submodules — [`chunk`] readers/wire helpers,
//! [`query`] scalar/admin reduces, [`agg`] TPUT top-K, [`ranked`]
//! MATCH/KNN/HYBRID).

mod advise;
mod agg;
mod chunk;
mod claused;
mod query;
mod ranked;

pub(crate) use chunk::{
    encode_view_cursor_bytes, read_kbytes_at, read_u32_at, resp3_upgrade, value_repr_pub,
};

use kevy_resp::encode_error;
use kevy_rt::ExtensionReduced;

use crate::cmd_index_query::{
    ST_BADARGS, ST_BUILDING, ST_CLAUSE, ST_NOFIELD, ST_NOINDEX, ST_NOTYET, ST_OVERBUDGET,
};
use crate::state::CatalogState;

/// Origin half: merge chunks → RESP (or a follow-up fan-out — the
/// GROUPS top-K and its AGG.FETCH phase are the two-phase shapes).
pub(crate) fn extension_reduce(
    catalogs: &CatalogState,
    argv: &[Vec<u8>],
    chunks: Vec<Vec<u8>>,
) -> ExtensionReduced {
    if let Some(err) = triage_status(argv, &chunks) {
        advise::observe_refusal(catalogs, argv, &chunks);
        return ExtensionReduced::Reply(err);
    }
    advise::observe_hit(catalogs, argv);
    if let Some(reply) = reduce_admin(catalogs, argv, &chunks) {
        return ExtensionReduced::Reply(reply);
    }
    // GROUP/GROUPS: TPUT-style exact top-K (see reduce_agg).
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"AGG.FETCH")) {
        return ExtensionReduced::Reply(agg::reduce_agg_fetch(argv, &chunks));
    }
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"GROUP") || a.eq_ignore_ascii_case(b"GROUPS")) {
        return agg::reduce_agg(argv, &chunks);
    }
    // KNN: merge distance-ranked chunks ascending.
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"KNN")) {
        return ExtensionReduced::Reply(ranked::reduce_ranked(argv, &chunks, true));
    }
    // REBUILD: all shards OK → +OK.
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"IDX.REBUILD")) {
        return ExtensionReduced::Reply(query::reduce_rebuild(&chunks));
    }
    // HYBRID: reciprocal-rank fusion of the two ranked segments.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) {
        return ExtensionReduced::Reply(ranked::reduce_hybrid(argv, &chunks));
    }
    // MATCH pass 2 (internal): merge the globally-scored ranked chunks.
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"MATCH.SCORE")) {
        return ExtensionReduced::Reply(ranked::reduce_match_score(argv, &chunks));
    }
    // MATCH pass 1: fold each shard's corpus counters into one global
    // CorpusStats, then re-fan-out MATCH.SCORE to score against it.
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"MATCH")) {
        return ranked::reduce_match_stats(argv, &chunks);
    }
    // IDX.QUERY COMPOSE: merge key-ordered chunks.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"COMPOSE")) {
        return ExtensionReduced::Reply(query::reduce_compose(argv, &chunks));
    }
    // IDX.QUERY: k-way merge by (value, key), global LIMIT + cursor.
    ExtensionReduced::Reply(query::reduce_query(argv, &chunks))
}

/// The admin verbs (EXPLAIN / COUNT / LIST / VERIFY); `None` = a
/// query-shaped verb, handled by the caller's shape dispatch.
fn reduce_admin(
    catalogs: &CatalogState,
    argv: &[Vec<u8>],
    chunks: &[Vec<u8>],
) -> Option<Vec<u8>> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    // IDX.EXPLAIN — pair-array plan summary.
    if verb.eq_ignore_ascii_case(b"IDX.EXPLAIN") {
        return Some(query::reduce_explain(catalogs, argv, chunks));
    }
    if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        return Some(query::reduce_count(chunks));
    }
    if verb.eq_ignore_ascii_case(b"IDX.LIST") {
        return Some(query::reduce_list(catalogs, chunks));
    }
    if verb.eq_ignore_ascii_case(b"IDX.VERIFY") {
        return Some(query::reduce_verify(chunks));
    }
    None
}

/// Status triage: any BADARGS / NOINDEX / BUILDING wins the reply.
/// Errors are SELF-EXPLAINING — they name the verb and the
/// index and point at the discovery surface, so an agent that hits
/// one can recover without out-of-band knowledge.
fn triage_status(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Option<Vec<u8>> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let verb_s = String::from_utf8_lossy(verb);
    let name_i = if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) { 2 } else { 1 };
    let name_s =
        argv.get(name_i).map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
    for c in chunks {
        if let Some(msg) = status_error(c, &verb_s, &name_s) {
            let mut out = Vec::new();
            encode_error(&mut out, &msg);
            return Some(out);
        }
    }
    None
}

/// The error one status-tagged chunk stands for, or `None` when the shard
/// answered normally. Each status gets its own sentence: collapsing them
/// would send people hunting for a typo in correct syntax.
fn status_error(c: &[u8], verb_s: &str, name_s: &str) -> Option<String> {
    match c.first().copied() {
        Some(ST_BADARGS) | None => Some(format!(
            "ERR {verb_s} '{name_s}': bad arguments — run COMMAND DOCS {verb_s} for the syntax"
        )),
        Some(ST_NOTYET) => {
            let clause = String::from_utf8_lossy(&c[1..]);
            Some(format!(
                "ERR {verb_s} '{name_s}': the {clause} clause is accepted by the parser but not implemented yet — it is part of the text-search arc, and silently ignoring it would give you wrong results rather than an error"
            ))
        }
        Some(ST_CLAUSE) => Some(format!(
            "ERR {verb_s} '{name_s}': {}",
            String::from_utf8_lossy(&c[1..])
        )),
        Some(ST_NOFIELD) => {
            let flen = c.get(1).copied().unwrap_or(0) as usize;
            let msg = c.get(2 + flen..).unwrap_or(b"");
            Some(format!("ERR {verb_s} '{name_s}': {}", String::from_utf8_lossy(msg)))
        }
        Some(ST_NOINDEX) => {
            Some(format!("ERR no such index '{name_s}' (IDX.LIST enumerates them)"))
        }
        Some(ST_BUILDING) => Some(format!(
            "INDEXBUILDING index '{name_s}' is still building (poll IDX.LIST until state=ready)"
        )),
        Some(ST_OVERBUDGET) => Some(format!(
            "INDEXOVERBUDGET index '{name_s}' build exceeded MAXMEM (raise maxmemory or DROP the index)"
        )),
        _ => None,
    }
}
