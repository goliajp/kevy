//! Embedded `TABLE.*` capability — the declaration
//! object, the compile call and the verify sweep. The grammar, the
//! validation and [`kevy_index::compile_table`] all live in
//! `kevy-index`: ONE implementation shared with the server, so the two
//! engines cannot compile a table differently (the IDX.CREATE parity
//! lesson; the dispatch oracle byte-compares the wire faces anyway).

use std::sync::{Mutex, RwLock};

use kevy_index::{
    AdviseLog, IndexVerify, TableCatalog, TableEnsure, TableSpec, TableVerify, compile_table,
};

use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

/// Store-level table state (declarations only — a table's runtime
/// footprint is its compiled indexes in the index registry).
#[derive(Default)]
pub(crate) struct TableReg {
    pub(crate) catalog: RwLock<TableCatalog>,
    /// The refusal log (the auto-declaration loop's observation
    /// face) — fed by the typed query API's refusals, rendered by
    /// [`Store::idx_advise`], cleared on every catalog mutation.
    pub(crate) advise: Mutex<AdviseLog>,
}

/// Rows the per-shard column spot check samples (mirrors the server).
const SPOTCHECK_ROWS: usize = 64;

#[cfg(feature = "persist")]
const SIDECAR: &str = "table-catalog.meta";

/// One `TABLE.VERIFY` result: per compiled index its name + six
/// counters (entries, bytes, coerce_failures, duplicates, drift,
/// checked), plus the `(rows, type_mismatches)` spot-check pair.
/// The legacy verify shape: six unnamed counters per index and two for
/// the spot check. Kept for semver; superseded by [`TableVerify`],
/// whose fields carry the names and time semantics these arrays never
/// could (dogfood F10).
#[deprecated(since = "4.1.0", note = "use `table_verify_report`, whose fields are named")]
pub type TableVerifyReport = (Vec<(Vec<u8>, [u64; 6])>, [u64; 2]);

impl Store {
    /// `TABLE.DECLARE` equivalent: admit the declaration, then build
    /// every compiled index synchronously. Atomic against catalog
    /// errors: names are dry-run against a catalog clone first, so a
    /// collision installs nothing.
    pub fn table_declare(&self, spec: TableSpec) -> KevyResult<()> {
        // Tiering floor refusal — the same precheck IDX.CREATE runs,
        // moved ahead of the catalog mutation so a refused declare
        // installs nothing.
        #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
        crate::ops_index_sync::tier_floor_check(&self.shards)?;
        // compile_table validates for itself — a bad spec is a named
        // refusal here, never a panic downstream (dogfood F9).
        let compiled = compile_table(&spec).map_err(KevyError::InvalidInput)?;
        {
            let g = self
                .tables
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if g.get(&spec.name).is_some() {
                return Err(KevyError::InvalidInput("table already exists".into()));
            }
        }
        // Dry-run the compiled specs against a clone of the index
        // catalog — the server admits into a clone and installs once;
        // this is the same all-or-nothing shape.
        {
            let g = self
                .indexes
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut probe = g.1.clone();
            for ispec in &compiled {
                probe
                    .create(ispec.clone())
                    .map_err(|e| KevyError::InvalidInput(strip_err(e).into()))?;
            }
        }
        {
            let mut g = self
                .tables
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.create(spec).map_err(|e| KevyError::InvalidInput(strip_err(&e).into()))?;
        }
        self.persist_table_sidecar();
        for ispec in compiled {
            self.register_spec(ispec)?;
        }
        self.advise_clear();
        Ok(())
    }

    /// `TABLE.ENSURE` equivalent — the boot verb.
    ///
    /// The dogfood report's F8.2: declaring at boot is the steady
    /// state, and plain `table_declare` punishes it — a re-declare is
    /// an error, so the obvious code (declare, log the error at debug)
    /// keeps running against whatever shape the *first* boot declared,
    /// forever, silently. `ensure` gives the boot path its true verb:
    /// an identical spec is a no-op success, a changed spec is a named
    /// refusal carrying what changed — never a silent rebuild, because
    /// a rebuild is a full backfill and must be asked for by name
    /// ([`Self::table_replace`]).
    pub fn table_ensure(&self, spec: TableSpec) -> KevyResult<TableEnsure> {
        let existing = {
            let g = self
                .tables
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.get(&spec.name).cloned()
        };
        match existing {
            None => {
                self.table_declare(spec)?;
                Ok(TableEnsure::Created)
            }
            Some(cur) if cur.sans_auto() == spec => Ok(TableEnsure::Unchanged),
            Some(cur) => Err(KevyError::InvalidInput(
                kevy_index::spec_diff(&cur.sans_auto(), &spec),
            )),
        }
    }

    /// `TABLE.REPLACE` equivalent — drop and redeclare, rebuilding
    /// every compiled index from the rows. Explicitly named because it
    /// is a full backfill: the cost is asked for, not implied. The new
    /// spec is compiled (and therefore validated) *before* the old
    /// table is dropped, so a bad replacement leaves the old one
    /// standing.
    pub fn table_replace(&self, spec: TableSpec) -> KevyResult<()> {
        compile_table(&spec).map_err(KevyError::InvalidInput)?;
        self.table_drop(&spec.name);
        self.table_declare(spec)
    }

    /// `TABLE.DROP` equivalent — drops the table AND its compiled
    /// indexes; `false` if absent.
    pub fn table_drop(&self, name: &[u8]) -> bool {
        let compiled: Vec<Vec<u8>> = {
            let g = self
                .tables
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.get(name)
                .map(|s| {
                    compile_table(s)
                        .map(|c| c.into_iter().map(|i| i.name).collect())
                        .unwrap_or_default() // catalog entries were admitted validated
                })
                .unwrap_or_default()
        };
        let hit = {
            let mut g = self
                .tables
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.drop_table(name)
        };
        if hit {
            for iname in &compiled {
                self.idx_drop(iname);
            }
            self.persist_table_sidecar();
            self.advise_clear();
        }
        hit
    }

    /// Declared tables, declaration order.
    pub fn table_list(&self) -> Vec<TableSpec> {
        let g = self
            .tables
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.iter().cloned().collect()
    }

    /// `TABLE.VERIFY` equivalent: each compiled index's full drift
    /// recheck (the IDX.VERIFY discipline — composites re-derive their
    /// byte encoding) plus a bounded column-type spot check, both
    /// through the no-promote peek.
    #[deprecated(since = "4.1.0", note = "use `table_verify_report`, whose fields are named")]
    #[allow(deprecated)]
    pub fn table_verify(&self, name: &[u8]) -> KevyResult<TableVerifyReport> {
        let r = self.table_verify_report(name)?;
        Ok((
            r.per_index
                .into_iter()
                .map(|i| {
                    (i.name, [i.entries, i.approx_bytes, i.coerce_failures, i.duplicates, i.drift, i.checked])
                })
                .collect(),
            [r.spot_rows, r.spot_type_mismatches],
        ))
    }

    /// `TABLE.VERIFY`, with every counter named and its time semantics
    /// documented on the field (see [`IndexVerify`]).
    pub fn table_verify_report(&self, name: &[u8]) -> KevyResult<TableVerify> {
        let spec = {
            let g = self
                .tables
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.get(name).cloned()
        }
        .ok_or_else(|| KevyError::NotFound("no such table".into()))?;
        let compiled = compile_table(&spec).map_err(KevyError::InvalidInput)?;
        let mut per_index: Vec<(Vec<u8>, [u64; 10])> =
            compiled.iter().map(|i| (i.name.clone(), [0u64; 10])).collect();
        let mut spot = [0u64; 2];
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            crate::ops_index_sync::sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            for (slot, ispec) in per_index.iter_mut().zip(&compiled) {
                shard_index_counts(inner, ispec, &mut slot.1);
            }
            let (r, m) = shard_spot_check(&mut inner.store, &spec);
            spot[0] += r;
            spot[1] += m;
        }
        Ok(TableVerify {
            per_index: per_index
                .into_iter()
                .map(|(name, s)| IndexVerify {
                    name,
                    entries: s[0],
                    approx_bytes: s[1],
                    coerce_failures: s[2],
                    duplicates: s[3],
                    drift: s[4],
                    checked: s[5],
                    excluded: s[6],
                    absent: s[7],
                    rows: s[8],
                    missing: s[9],
                })
                .collect(),
            spot_rows: spot[0],
            spot_type_mismatches: spot[1],
        })
    }

    #[cfg(feature = "persist")]
    pub(crate) fn table_boot(&self) {
        let Some(dir) = &self.config.data_dir else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join(SIDECAR))
            && let Some(cat) = TableCatalog::from_sidecar(&text)
            && !cat.is_empty()
        {
            let mut g = self
                .tables
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *g = cat;
        }
    }

    #[cfg(not(feature = "persist"))]
    pub(crate) fn table_boot(&self) {}

    #[cfg(feature = "persist")]
    pub(crate) fn persist_table_sidecar(&self) {
        let Some(dir) = &self.config.data_dir else { return };
        let g = self
            .tables
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = dir.join("table-catalog.meta.tmp");
        if std::fs::write(&tmp, g.to_sidecar()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
        }
    }

    #[cfg(not(feature = "persist"))]
    pub(crate) fn persist_table_sidecar(&self) {}
}

/// Catalog errors carry a leading `ERR ` for the wire; the typed
/// `KevyError` face re-adds it, so strip here to avoid `ERR ERR`.
fn strip_err(e: &str) -> &str {
    e.strip_prefix("ERR ").unwrap_or(e)
}

/// One shard's contribution to one compiled index's verify counters —
/// both directions.
///
/// index→row (`drift`): every held entry re-derives from its row.
/// row→index: every prefix row classifies against the spec,
/// with the cause kept — `absent` (NULL, by design), `coerce_failures`
/// (present-but-wrong-type, fresh: the 4.0 lifetime counter of this
/// name also swallowed absences), `excluded` (composite oversize, the
/// silent two-row gap of dogfood F8/F9, now a named number), and
/// `missing` (derives a value, has no entry — the class a drift walk
/// structurally cannot see, and the one behind F13/F14's forgotten
/// writers).
///
/// Layout: [entries, bytes, coerce_fresh, duplicates, drift, checked,
///          excluded, absent, rows, missing].
/// The row→index half of the walk, under the no-promote peek: every
/// prefix row classified by cause (the server's `classify_prefix_rows`
/// is the byte-parity twin).
/// Returns `[coerce_fresh, excluded, absent, rows, missing]`.
/// `hot_floor` is `(boundary, shape)` for a windowed path — the twin of
/// the server's `cmd_table_verify::classify_prefix_rows`, including its
/// reason: a row that slid to a cold segment is absent from the hot
/// entries by design, and counting it as a hole would make every
/// windowed table report its own sliding as corruption.
fn classify_prefix_rows(
    s: &mut kevy_store::Store,
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
            // A non-hash or vanished row has no columns: NULL row.
            _ => kevy_index::RowDerivation::Absent,
        };
        match cls {
            kevy_index::RowDerivation::Indexed(_) => {
                let slid = hot_floor.is_some_and(|(boundary, shape)| {
                    let v = match s.peek_hash_fields(key, &names[..w]) {
                        Ok(Some(vals)) => spec.derive_scalar(&vals),
                        _ => None,
                    };
                    v.and_then(|v| kevy_index::window_value_of(&v, shape))
                        .is_some_and(|wv| wv < boundary)
                });
                if !slid && !indexed.contains(key.as_slice()) {
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

/// The window boundary a `missing` count must not fault rows against:
/// a row below it slid into a cold segment on purpose. `None` when the
/// path is unwindowed or has not slid yet (and always on wasm, where
/// nothing slides).
fn hot_floor_of(
    inner: &crate::store_inner::Inner,
    name: &[u8],
) -> Option<(i64, kevy_index::WindowShape)> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        inner
            .idx_segs
            .windows
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, w)| (w.boundary(), w.shape))
            .filter(|(b, _)| *b != i64::MIN)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (inner, name);
        None
    }
}

fn shard_index_counts(
    inner: &mut crate::store_inner::Inner,
    ispec: &kevy_index::IndexSpec,
    sums: &mut [u64; 10],
) {
    let Some((spec, seg)) = inner.idx_segs.segs.iter().find(|(s, _)| s.name == ispec.name)
    else {
        return;
    };
    let stats = seg.stats();
    let mut entries: Vec<(Vec<u8>, kevy_index::IndexValue)> = Vec::new();
    seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
    let indexed: std::collections::HashSet<&[u8]> =
        entries.iter().map(|(k, _)| k.as_slice()).collect();
    let spec = spec.clone();
    let mut pat = spec.prefix.clone();
    pat.push(b'*');
    let row_keys = inner.store.collect_keys(Some(&pat), None);
    let hot_floor = hot_floor_of(inner, &spec.name);
    let store = &mut inner.store;
    let (drift, fresh) = store.peek_scope(|s| {
        let names = spec.scalar_read_names();
        let w = spec.primary_width();
        let mut drift = 0u64;
        for (key, held) in &entries {
            let actual = match s.peek_hash_fields(key, &names[..w]) {
                Ok(Some(vals)) => spec.derive_scalar(&vals),
                _ => None,
            };
            if actual.as_ref() != Some(held) {
                drift += 1;
            }
        }
        (drift, classify_prefix_rows(s, &spec, &row_keys, &indexed, hot_floor))
    });
    sums[0] += stats.entries;
    sums[1] += stats.approx_bytes;
    sums[2] += fresh[0];
    sums[3] += stats.duplicates;
    sums[4] += drift;
    sums[5] += entries.len() as u64;
    sums[6] += fresh[1];
    sums[7] += fresh[2];
    sums[8] += fresh[3];
    sums[9] += fresh[4];
}

/// Sample up to [`SPOTCHECK_ROWS`] rows on one shard: every PRESENT
/// declared-typed column must coerce (absent = NULL — Law 3; a
/// non-hash row counts as a row with no columns).
fn shard_spot_check(store: &mut kevy_store::Store, spec: &TableSpec) -> (u64, u64) {
    let mut pat = spec.prefix.clone();
    pat.push(b'*');
    let keys = store.collect_keys(Some(&pat), Some(SPOTCHECK_ROWS));
    let names: Vec<&[u8]> = spec.columns.iter().map(|(n, _)| n.as_slice()).collect();
    store.peek_scope(|s| {
        let (mut rows, mut mismatches) = (0u64, 0u64);
        for key in &keys {
            rows += 1;
            let Ok(Some(vals)) = s.peek_hash_fields(key, &names) else { continue };
            for ((_, ty), val) in spec.columns.iter().zip(&vals) {
                if let Some(raw) = val
                    && kevy_index::IndexValue::coerce(*ty, raw).is_none()
                {
                    mismatches += 1;
                }
            }
        }
        (rows, mismatches)
    })
}
