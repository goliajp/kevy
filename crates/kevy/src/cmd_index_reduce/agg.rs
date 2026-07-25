//! GROUP/GROUPS reduce — distributed EXACT top-K over group
//! aggregates (TPUT-style) plus the AGG.FETCH phase-2 merge.

use kevy_resp::{encode_array_len, encode_bulk, encode_error};
use kevy_rt::ExtensionReduced;

use super::chunk::{read_kbytes, read_u32, value_repr};

/// Reduce — distributed EXACT top-K over group aggregates
/// (TPUT-style). Phase 1 chunks carry each shard's local top-(4·limit)
/// by the ranking metric plus an `exhausted` flag; if the unseen mass
/// (Σ τ over non-exhausted shards) could displace the k-th candidate,
/// a continuation re-runs phase 1 with 4× depth (terminates: depth
/// reaches every group). Otherwise a targeted AGG.FETCH continuation
/// collects the survivors' exact partials from every shard.
///
/// History (agggate): full-materialization chunks 14-18ms at 8×10k
/// groups; this path answers from ~4·limit-row chunks.
pub(super) fn reduce_agg(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> ExtensionReduced {
    let mut out = Vec::new();
    let single = argv[2].eq_ignore_ascii_case(b"GROUP");
    if single {
        return ExtensionReduced::Reply(reduce_agg_single(chunks));
    }
    let Some((by, limit)) = crate::cmd_index_query::parse_groups_args(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return ExtensionReduced::Reply(out);
    };
    // ---- phase 1: merge observed partials + collect per-shard τ ----
    let additive = matches!(by, kevy_index::AggBy::Count | kevy_index::AggBy::Sum);
    let (observed, taus) = collect_partials(chunks, by);
    // rank observed by score; θ = k-th best observed score
    let mut ranked: Vec<(Vec<u8>, kevy_index::GroupStats)> = observed.into_iter().collect();
    ranked.sort_by(|a, b| score(&b.1, by).total_cmp(&score(&a.1, by)).then_with(|| a.0.cmp(&b.0)));
    let theta = ranked
        .get(limit - 1)
        .map_or(f64::NEG_INFINITY, |(_, st)| score(st, by));
    // uncertainty: could anything UNSEEN (or unseen mass of a seen
    // group) displace the k-th? Additive: bound = Σ τ; max-type:
    // bound = max τ.
    let unseen_bound = if additive {
        taus.iter().sum::<f64>()
    } else {
        taus.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    };
    let depth = groups_depth(argv);
    if unseen_bound > theta && depth != 0 {
        // One 4× deepening, then jump STRAIGHT to full
        // materialization (DEPTH=0): uniform near-tie data defeats
        // thresholds information-theoretically, and a geometric crawl
        // just multiplies fan-out rounds (measured 51ms).
        let mut argv2: Vec<Vec<u8>> = argv.to_vec();
        set_groups_depth(&mut argv2, if depth == 1 { 4 } else { 0 });
        return continuation(argv2);
    }
    // ---- phase 2: exact totals for potential top-k members ----
    let cands = fetch_candidates(&ranked, &taus, theta, by, additive, unseen_bound, limit);
    let mut argv2: Vec<Vec<u8>> = vec![
        b"AGG.FETCH".to_vec(),
        argv[1].clone(),
        format!("BY={} LIMIT={}", tag_of(by), limit).into_bytes(),
    ];
    argv2.extend(cands);
    continuation(argv2)
}

/// GROUP (one group): chunks are exact partials already — merge & emit.
fn reduce_agg_single(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut st = kevy_index::GroupStats { count: 0, sum: 0.0, min: None, max: None };
    for c in chunks {
        for (_g, part) in decode_agg_chunk(c) {
            kevy_index::merge_group(&mut st, &part);
        }
    }
    encode_array_len(&mut out, 5);
    encode_bulk(&mut out, st.count.to_string().as_bytes());
    encode_bulk(&mut out, format!("{}", st.sum).as_bytes());
    for v in [&st.min, &st.max] {
        match v {
            Some(x) => encode_bulk(&mut out, &value_repr(x)),
            None => out.extend_from_slice(b"$-1\r\n"),
        }
    }
    match st.avg() {
        Some(a) => encode_bulk(&mut out, format!("{a}").as_bytes()),
        None => out.extend_from_slice(b"$-1\r\n"),
    }
    out
}

/// Phase-1 merge: fold every shard's observed partials and collect the
/// score bound τ per NON-exhausted shard.
fn collect_partials(
    chunks: &[Vec<u8>],
    by: kevy_index::AggBy,
) -> (std::collections::HashMap<Vec<u8>, kevy_index::GroupStats>, Vec<f64>) {
    let mut observed: std::collections::HashMap<Vec<u8>, kevy_index::GroupStats> =
        std::collections::HashMap::new();
    let mut taus: Vec<f64> = Vec::new(); // score bound per NON-exhausted shard
    for c in chunks {
        let rows = decode_agg_chunk(&c[..c.len().saturating_sub(1)]);
        let exhausted = c.last() == Some(&1);
        if !exhausted {
            let tau = rows.last().map_or(f64::NEG_INFINITY, |(_, st)| score(st, by));
            taus.push(tau);
        }
        for (g, part) in rows {
            match observed.get_mut(&g) {
                Some(st) => kevy_index::merge_group(st, &part),
                None => {
                    observed.insert(g, part);
                }
            }
        }
    }
    (observed, taus)
}

/// Phase-2 candidate set: observed groups whose upper bound reaches θ,
/// hard-capped on fetch width.
fn fetch_candidates(
    ranked: &[(Vec<u8>, kevy_index::GroupStats)],
    taus: &[f64],
    theta: f64,
    by: kevy_index::AggBy,
    additive: bool,
    unseen_bound: f64,
    limit: usize,
) -> Vec<Vec<u8>> {
    let mut cands: Vec<Vec<u8>> = ranked
        .iter()
        .filter(|(_, st)| {
            let upper = if additive {
                score(st, by) + taus.iter().sum::<f64>()
            } else {
                score(st, by).max(unseen_bound)
            };
            upper >= theta
        })
        .map(|(g, _)| g.clone())
        .collect();
    cands.truncate((limit * 32).max(256)); // hard cap on fetch width
    cands
}

/// Final reduce for the AGG.FETCH phase: exact merge of the fetched
/// candidates, rank, truncate, emit.
pub(super) fn reduce_agg_fetch(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let meta = argv.get(2).map(|m| String::from_utf8_lossy(m).into_owned()).unwrap_or_default();
    let by = meta
        .split_whitespace()
        .find_map(|t| t.strip_prefix("BY=").and_then(|b| kevy_index::AggBy::parse(b.as_bytes())))
        .unwrap_or_default();
    let limit: usize = meta
        .split_whitespace()
        .find_map(|t| t.strip_prefix("LIMIT=").and_then(|n| n.parse().ok()))
        .unwrap_or(100);
    let mut merged: std::collections::HashMap<Vec<u8>, kevy_index::GroupStats> =
        std::collections::HashMap::new();
    for c in chunks {
        for (g, part) in decode_agg_chunk(c) {
            match merged.get_mut(&g) {
                Some(st) => kevy_index::merge_group(st, &part),
                None => {
                    merged.insert(g, part);
                }
            }
        }
    }
    let mut ranked: Vec<(Vec<u8>, kevy_index::GroupStats)> = merged
        .into_iter()
        .filter(|(_, st)| st.count > 0)
        .collect();
    kevy_index::sort_groups(&mut ranked, by);
    ranked.truncate(limit);
    encode_array_len(&mut out, ranked.len() as i64);
    for (g, st) in &ranked {
        encode_array_len(&mut out, 5);
        encode_bulk(&mut out, g);
        encode_bulk(&mut out, st.count.to_string().as_bytes());
        encode_bulk(&mut out, format!("{}", st.sum).as_bytes());
        for v in [&st.min, &st.max] {
            match v {
                Some(x) => encode_bulk(&mut out, &value_repr(x)),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
    }
    out
}

/// Ranking score, oriented bigger-is-better for every metric.
fn score(st: &kevy_index::GroupStats, by: kevy_index::AggBy) -> f64 {
    match by {
        kevy_index::AggBy::Count => st.count as f64,
        kevy_index::AggBy::Sum => st.sum,
        kevy_index::AggBy::Max => st.max.as_ref().map_or(f64::NEG_INFINITY, |v| v.as_f64()),
        kevy_index::AggBy::Min => st.min.as_ref().map_or(f64::NEG_INFINITY, |v| -v.as_f64()),
    }
}

fn tag_of(by: kevy_index::AggBy) -> &'static str {
    match by {
        kevy_index::AggBy::Count => "count",
        kevy_index::AggBy::Sum => "sum",
        kevy_index::AggBy::Min => "min",
        kevy_index::AggBy::Max => "max",
    }
}

/// Internal DEPTH arg (iterative deepening), default 1.
fn groups_depth(argv: &[Vec<u8>]) -> usize {
    argv.iter()
        .find_map(|a| {
            std::str::from_utf8(a).ok()?.strip_prefix("DEPTH=")?.parse().ok()
        })
        .unwrap_or(1)
}

fn set_groups_depth(argv: &mut Vec<Vec<u8>>, depth: usize) {
    for a in argv.iter_mut() {
        if a.starts_with(b"DEPTH=") {
            *a = format!("DEPTH={depth}").into_bytes();
            return;
        }
    }
    argv.push(format!("DEPTH={depth}").into_bytes());
}

/// Stateless two-phase follow-up: hand the runtime the argv to re-fan
/// out (phase state rides inside the argv itself).
fn continuation(argv: Vec<Vec<u8>>) -> ExtensionReduced {
    ExtensionReduced::Continue(argv)
}

/// Decode `[ST_OK][n][(glen,g,count,sum,mmlen,mm)*]`.
fn decode_agg_chunk(c: &[u8]) -> Vec<(Vec<u8>, kevy_index::GroupStats)> {
    let mut rows = Vec::new();
    let mut pos = 1usize;
    let Some(n) = read_u32(c, &mut pos) else { return rows };
    for _ in 0..n {
        let Some(g) = read_kbytes(c, &mut pos) else { break };
        let Some(cb) = c.get(pos..pos + 8) else { break };
        let count = u64::from_le_bytes(cb.try_into().expect("8"));
        pos += 8;
        let Some(sb) = c.get(pos..pos + 8) else { break };
        let sum = f64::from_le_bytes(sb.try_into().expect("8"));
        pos += 8;
        let Some(ml) = c.get(pos..pos + 4) else { break };
        let ml = u32::from_le_bytes(ml.try_into().expect("4")) as usize;
        pos += 4;
        let Some(mm) = c.get(pos..pos + ml) else { break };
        pos += ml;
        let mut mpos = 0usize;
        let mut vals = [None, None];
        for slot in &mut vals {
            match mm.get(mpos).copied() {
                Some(1) => {
                    mpos += 1;
                    *slot = crate::cmd_index_query::decode_value(mm, &mut mpos);
                }
                _ => mpos += 1,
            }
        }
        rows.push((g, kevy_index::GroupStats { count, sum, min: vals[0].clone(), max: vals[1].clone() }));
    }
    rows
}
