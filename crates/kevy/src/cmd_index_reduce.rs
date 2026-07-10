//! v2.5 — IDX.* origin-side reduce: merge per-shard chunks into RESP
//! (split from [`crate::cmd_index_query`] under the 500-LOC rule;
//! v3.18 re-split into submodules — [`chunk`] readers/wire helpers,
//! [`query`] scalar/admin reduces, [`agg`] TPUT top-K, [`ranked`]
//! MATCH/KNN/HYBRID).

mod agg;
mod chunk;
mod query;
mod ranked;

pub(crate) use chunk::{
    encode_view_cursor_bytes, read_kbytes_at, read_u32_at, resp3_upgrade, value_repr_pub,
};

use kevy_resp::encode_error;

use crate::cmd_index_query::{ST_BADARGS, ST_BUILDING, ST_NOINDEX, ST_OVERBUDGET};

/// Origin half: merge chunks → RESP.
pub(crate) fn extension_reduce(argv: &[Vec<u8>], chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if let Some(err) = triage_status(argv, &chunks) {
        return err;
    }
    // v3.10: IDX.EXPLAIN — pair-array plan summary.
    if verb.eq_ignore_ascii_case(b"IDX.EXPLAIN") {
        return query::reduce_explain(argv, &chunks);
    }
    if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        return query::reduce_count(&chunks);
    }
    if verb.eq_ignore_ascii_case(b"IDX.LIST") {
        return query::reduce_list(&chunks);
    }
    if verb.eq_ignore_ascii_case(b"IDX.VERIFY") {
        return query::reduce_verify(&chunks);
    }
    // v3.1 GROUP/GROUPS: TPUT-style exact top-K (see reduce_agg).
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"AGG.FETCH")) {
        return agg::reduce_agg_fetch(argv, &chunks);
    }
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"GROUP") || a.eq_ignore_ascii_case(b"GROUPS")) {
        return agg::reduce_agg(argv, &chunks);
    }
    // v2.8 KNN: merge distance-ranked chunks ascending.
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"KNN")) {
        return ranked::reduce_ranked(argv, &chunks, true);
    }
    // v2.8 REBUILD: all shards OK → +OK.
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"IDX.REBUILD")) {
        return query::reduce_rebuild(&chunks);
    }
    // v3.13 HYBRID: reciprocal-rank fusion of the two ranked segments.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) {
        return ranked::reduce_hybrid(argv, &chunks);
    }
    // v2.7 MATCH: same chunk layout as KNN, sorted score-descending.
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"MATCH")) {
        return ranked::reduce_ranked(argv, &chunks, false);
    }
    // IDX.QUERY COMPOSE: merge key-ordered chunks.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"COMPOSE")) {
        return query::reduce_compose(argv, &chunks);
    }
    // IDX.QUERY: k-way merge by (value, key), global LIMIT + cursor.
    query::reduce_query(argv, &chunks)
}

/// Status triage: any BADARGS / NOINDEX / BUILDING wins the reply.
/// v3.10: errors are SELF-EXPLAINING — they name the verb and the
/// index and point at the discovery surface, so an agent that hits
/// one can recover without out-of-band knowledge.
fn triage_status(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Option<Vec<u8>> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let verb_s = String::from_utf8_lossy(verb);
    let name_i = if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) { 2 } else { 1 };
    let name_s = argv.get(name_i).map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
    let mut out = Vec::new();
    for c in chunks {
        match c.first().copied() {
            Some(ST_BADARGS) | None => {
                encode_error(
                    &mut out,
                    &format!("ERR {verb_s} '{name_s}': bad arguments — run COMMAND DOCS {verb_s} for the syntax"),
                );
                return Some(out);
            }
            Some(ST_NOINDEX) => {
                encode_error(
                    &mut out,
                    &format!("ERR no such index '{name_s}' (IDX.LIST enumerates them)"),
                );
                return Some(out);
            }
            Some(ST_BUILDING) => {
                encode_error(
                    &mut out,
                    &format!("INDEXBUILDING index '{name_s}' is still building (poll IDX.LIST until state=ready)"),
                );
                return Some(out);
            }
            Some(ST_OVERBUDGET) => {
                encode_error(
                    &mut out,
                    &format!("INDEXOVERBUDGET index '{name_s}' build exceeded MAXMEM (raise maxmemory or DROP the index)"),
                );
                return Some(out);
            }
            _ => {}
        }
    }
    None
}
