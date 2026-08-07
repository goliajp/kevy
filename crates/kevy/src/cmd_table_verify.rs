//! `TABLE.VERIFY`'s per-index counters — both directions of the audit.
//! Split from `cmd_table` for the 500-LOC house rule.

use kevy_store::Store;

use crate::index_runtime;
use crate::state::Ctx;

/// One compiled index's per-shard verify counters — both directions
/// (the embedded face's `shard_index_counts` is the byte-parity
/// twin, and the oracle holds them together).
///
/// index→row: the IDX.VERIFY drift recheck. row→index: every prefix row
/// classifies against the spec with the cause kept — fresh
/// `coerce_failures` (present-but-wrong-type only; the 4.0 lifetime
/// counter also swallowed absences), `excluded` (composite oversize),
/// `absent` (NULL by design), and `missing` (derives a value, has no
/// entry — the class a drift walk cannot see).
/// The row→index half of the walk, under the no-promote peek: every
/// prefix row classified by cause.
/// Returns `[coerce_fresh, excluded, absent, rows, missing]`.
/// `window` is the audit's cold-side evidence on a windowed path. Rows
/// below the boundary are absent from the hot tree by design — but only
/// if they reached a cold segment, and a row lost between the two
/// structures sits below the boundary exactly like a legitimate one.
/// So they are counted, not exempted, and reconciled against the live
/// cold entries at the end. Without any of this a windowed path reports
/// every slid row as a hole in its own index (17500 of them on a
/// 20000-row table); exempting them by position instead reports zero
/// even when rows really are lost.
pub(crate) fn classify_prefix_rows(
    s: &mut Store,
    spec: &kevy_index::IndexSpec,
    row_keys: &[Vec<u8>],
    indexed: &std::collections::HashSet<&[u8]>,
    window: Option<kevy_index::WindowAudit>,
) -> [u64; 5] {
    let names = spec.scalar_read_names();
    let w = spec.primary_width();
    let mut f = [0u64; 5];
    // Holes below the window boundary: candidates for the cold side,
    // reconciled against its own count once the walk is done.
    let mut below = 0u64;
    for key in row_keys {
        f[3] += 1;
        let cls = match s.peek_hash_fields(key, &names[..w]) {
            Ok(Some(vals)) => spec.classify_scalar(&vals),
            _ => kevy_index::RowDerivation::Absent,
        };
        match cls {
            kevy_index::RowDerivation::Indexed(_) => {
                if !indexed.contains(key.as_slice()) {
                    if slid_out(s, spec, key, window) {
                        below += 1;
                    } else {
                        f[4] += 1;
                    }
                }
            }
            kevy_index::RowDerivation::CoerceFailed => f[0] += 1,
            kevy_index::RowDerivation::Oversize => f[1] += 1,
            kevy_index::RowDerivation::Absent => f[2] += 1,
        }
    }
    // Every row that slid should have an entry waiting for it. The
    // shortfall is exactly the rows that fell between the hot tree and
    // the segment. `saturating_sub` because a cold entry can outlive
    // its row (tombstoning is the writer's job, not the reader's) —
    // that direction is the drift walk's, not this one's.
    f[4] += below.saturating_sub(window.map_or(0, |w| w.cold_live));
    f
}

/// Is this row below the window boundary — i.e. one the cold side is
/// supposed to be holding? Only asked about a row that looks missing,
/// so the extra peek is paid per candidate hole rather than per row.
/// Says nothing about whether the cold side actually has it; the
/// caller's reconciliation does that.
fn slid_out(
    s: &mut Store,
    spec: &kevy_index::IndexSpec,
    key: &[u8],
    window: Option<kevy_index::WindowAudit>,
) -> bool {
    let Some(w) = window else {
        return false;
    };
    match index_runtime::row_value(s, spec, key) {
        index_runtime::RowValue::Value(v) => {
            kevy_index::window_value_of(&v, w.shape).is_some_and(|wv| wv < w.boundary)
        }
        _ => false,
    }
}

pub(crate) fn index_verify_counts(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
) -> Result<[u64; 10], kevy_resp::CmdError> {
    let (spec, entries, stats, window) =
        index_runtime::with_ready_segment(ctx, store, name, |spec, seg, win| {
            let mut entries: Vec<(Vec<u8>, kevy_index::IndexValue)> = Vec::new();
            seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
            let audit = win.and_then(|w| w.audit(spec.ty));
            (spec.clone(), entries, seg.stats(), audit)
        })?;
    let indexed: std::collections::HashSet<&[u8]> =
        entries.iter().map(|(k, _)| k.as_slice()).collect();
    let mut pat = spec.prefix.clone();
    pat.push(b'*');
    let row_keys = store.collect_keys(Some(&pat), None);
    let (drift, fresh) = store.peek_scope(|s| {
        let mut drift = 0u64;
        for (key, held) in &entries {
            match index_runtime::row_value(s, &spec, key) {
                index_runtime::RowValue::Value(actual) if &actual == held => {}
                _ => drift += 1,
            }
        }
        (drift, classify_prefix_rows(s, &spec, &row_keys, &indexed, window))
    });
    Ok([
        stats.entries,
        stats.approx_bytes,
        fresh[0],
        stats.duplicates,
        drift,
        entries.len() as u64,
        fresh[1],
        fresh[2],
        fresh[3],
        fresh[4],
    ])
}
