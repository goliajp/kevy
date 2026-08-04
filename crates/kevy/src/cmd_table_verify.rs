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
/// `hot_floor` is `(boundary, shape)` for a windowed path: a row whose
/// window value sits below the boundary has legitimately slid to a cold
/// segment and is absent from the hot entries by design, so it is not
/// `missing`. Without it a windowed path reports every slid row as a
/// hole in its own index.
pub(crate) fn classify_prefix_rows(
    s: &mut Store,
    spec: &kevy_index::IndexSpec,
    row_keys: &[Vec<u8>],
    indexed: &std::collections::HashSet<&[u8]>,
    hot_floor: Option<(i64, kevy_index::WindowShape)>,
) -> [u64; 5] {
    let names = spec.scalar_read_names();
    let w = spec.primary_width();
    let mut f = [0u64; 5];
    for key in row_keys {
        f[3] += 1;
        let cls = match s.peek_hash_fields(key, &names[..w]) {
            Ok(Some(vals)) => spec.classify_scalar(&vals),
            _ => kevy_index::RowDerivation::Absent,
        };
        match cls {
            kevy_index::RowDerivation::Indexed(_) => {
                if !indexed.contains(key.as_slice()) && !slid_out(s, spec, key, hot_floor) {
                    f[4] += 1;
                }
            }
            kevy_index::RowDerivation::CoerceFailed => f[0] += 1,
            kevy_index::RowDerivation::Oversize => f[1] += 1,
            kevy_index::RowDerivation::Absent => f[2] += 1,
        }
    }
    f
}

/// Did this row leave the hot segment on purpose? Only asked about a
/// row that looks missing, so the extra peek is paid per candidate hole
/// rather than per row. The row's own indexed value decides — the same
/// value the segment would hold — against the window's boundary.
fn slid_out(
    s: &mut Store,
    spec: &kevy_index::IndexSpec,
    key: &[u8],
    hot_floor: Option<(i64, kevy_index::WindowShape)>,
) -> bool {
    let Some((boundary, shape)) = hot_floor else {
        return false;
    };
    match index_runtime::row_value(s, spec, key) {
        index_runtime::RowValue::Value(v) => {
            kevy_index::window_value_of(&v, shape).is_some_and(|wv| wv < boundary)
        }
        _ => false,
    }
}

pub(crate) fn index_verify_counts(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
) -> Result<[u64; 10], kevy_resp::CmdError> {
    let (spec, entries, stats, hot_floor) =
        index_runtime::with_ready_segment(ctx, store, name, |spec, seg, win| {
            let mut entries: Vec<(Vec<u8>, kevy_index::IndexValue)> = Vec::new();
            seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
            let floor = win
                .filter(|w| w.boundary() != i64::MIN)
                .map(|w| (w.boundary(), w.shape));
            (spec.clone(), entries, seg.stats(), floor)
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
        (drift, classify_prefix_rows(s, &spec, &row_keys, &indexed, hot_floor))
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
