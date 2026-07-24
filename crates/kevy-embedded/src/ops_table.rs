//! Embedded `TABLE.*` capability (capacity arc T7) — the declaration
//! object, the compile call and the verify sweep. The grammar, the
//! validation and [`kevy_index::compile_table`] all live in
//! `kevy-index`: ONE implementation shared with the server, so the two
//! engines cannot compile a table differently (the IDX.CREATE parity
//! lesson; the dispatch oracle byte-compares the wire faces anyway).

use std::sync::RwLock;

use kevy_index::{TableCatalog, TableSpec, compile_table};

use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

/// Store-level table state (declarations only — a table's runtime
/// footprint is its compiled indexes in the index registry).
#[derive(Default)]
pub(crate) struct TableReg {
    pub(crate) catalog: RwLock<TableCatalog>,
}

/// Rows the per-shard column spot check samples (mirrors the server).
const SPOTCHECK_ROWS: usize = 64;

#[cfg(feature = "persist")]
const SIDECAR: &str = "table-catalog.meta";

/// One `TABLE.VERIFY` result: per compiled index its name + six
/// counters (entries, bytes, coerce_failures, duplicates, drift,
/// checked), plus the `(rows, type_mismatches)` spot-check pair.
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
        let compiled = compile_table(&spec);
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
        Ok(())
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
                .map(|s| compile_table(s).into_iter().map(|i| i.name).collect())
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
    pub fn table_verify(&self, name: &[u8]) -> KevyResult<TableVerifyReport> {
        let spec = {
            let g = self
                .tables
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.get(name).cloned()
        }
        .ok_or_else(|| KevyError::NotFound("no such table".into()))?;
        let compiled = compile_table(&spec);
        let mut per_index: Vec<(Vec<u8>, [u64; 6])> =
            compiled.iter().map(|i| (i.name.clone(), [0u64; 6])).collect();
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
        Ok((per_index, spot))
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
    fn persist_table_sidecar(&self) {
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
    fn persist_table_sidecar(&self) {}
}

/// Catalog errors carry a leading `ERR ` for the wire; the typed
/// `KevyError` face re-adds it, so strip here to avoid `ERR ERR`.
fn strip_err(e: &str) -> &str {
    e.strip_prefix("ERR ").unwrap_or(e)
}

/// One shard's contribution to one compiled index's verify counters.
fn shard_index_counts(
    inner: &mut crate::store_inner::Inner,
    ispec: &kevy_index::IndexSpec,
    sums: &mut [u64; 6],
) {
    let Some((spec, seg)) = inner.idx_segs.segs.iter().find(|(s, _)| s.name == ispec.name)
    else {
        return;
    };
    let stats = seg.stats();
    let mut entries: Vec<(Vec<u8>, kevy_index::IndexValue)> = Vec::new();
    seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
    let spec = spec.clone();
    let store = &mut inner.store;
    let drift = store.peek_scope(|s| {
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
        drift
    });
    sums[0] += stats.entries;
    sums[1] += stats.approx_bytes;
    sums[2] += stats.coerce_failures;
    sums[3] += stats.duplicates;
    sums[4] += drift;
    sums[5] += entries.len() as u64;
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
