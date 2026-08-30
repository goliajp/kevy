//! The embedded face of the auto-declaration loop: the same bounded
//! refusal log the server keeps ([`kevy_index::AdviseLog`] — one
//! shared implementation, so the two faces cannot observe
//! differently), fed by the typed query API's refusals and rendered
//! by [`Store::idx_advise`]. A `#[path]` child of `ops_index.rs`.
//!
//! Two face-specific notes. The embedded API has no argv, so samples
//! are empty and a composite path asked for through
//! [`Store::idx_query`] is observed as a Range family — when its
//! suffix is not a column, the catalog withholds the advice, exactly
//! as the wire face does. And the text MATCH clause surface does not
//! feed the log yet — its unstored-field refusals belong to a text
//! declaration form the advice renderer does not speak (the
//! autodeclare RFC's slice b).

use std::sync::PoisonError;

use kevy_index::{AdviseShape, advice_of};

use crate::store::Store;
use crate::{KevyError, KevyResult};

/// Feed one windowed query's probe depth (`lower - boundary`) into
/// the path's cell — the window-narrowing observation. Skipped until
/// the boundary exists and for bounds outside the window shape; a
/// per-shard repeat just re-records the same minimum.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn probe_window(
    cell: &Option<std::sync::Arc<kevy_index::UsageCell>>,
    win: &kevy_window::WindowRt,
    lower: &kevy_index::IndexValue,
) {
    let Some(c) = cell else { return };
    if win.boundary() == i64::MIN {
        return;
    }
    if let Some(v) = kevy_index::window_value_of(lower, win.shape) {
        c.probe(v.saturating_sub(win.boundary()));
    }
}

/// One [`Store::idx_advise`] row: how often the family was refused,
/// the access path it asked for, and the declaration that serves it.
#[derive(Debug, Clone)]
pub struct IdxAdvice {
    /// Refusals observed for this family.
    pub count: u64,
    /// The access-path name the queries asked for.
    pub name: Vec<u8>,
    /// The declaration command that would have served them.
    pub advice: String,
}

impl Store {
    /// Observed refusal families, most-refused first, each rendered
    /// as the declaration that would have served it. Families the
    /// table catalog cannot ground (unknown table / column) are
    /// withheld — those queries were malformed, not under-declared.
    /// The log clears on every catalog mutation.
    pub fn idx_advise(&self) -> Vec<IdxAdvice> {
        let entries: Vec<kevy_index::AdviseEntry> = {
            let g = self.tables.advise.lock().unwrap_or_else(PoisonError::into_inner);
            g.entries().into_iter().cloned().collect()
        };
        let mut rows: Vec<IdxAdvice> = {
            let cat = self.tables.catalog.read().unwrap_or_else(PoisonError::into_inner);
            entries
                .iter()
                .filter_map(|e| {
                    advice_of(e, &cat).map(|advice| IdxAdvice {
                        count: e.count,
                        name: e.name.clone(),
                        advice,
                    })
                })
                .collect()
        };
        rows.extend(self.reclaim_rows());
        rows
    }

    /// The window-narrowing face, then the reclaim face: a windowed
    /// path whose every observed probe left more than a bucket of
    /// margin advises a smaller SPAN; a declared path no query has
    /// ever hit advises its own drop, with its age. Dropping and
    /// narrowing both stay human acts.
    fn reclaim_rows(&self) -> Vec<IdxAdvice> {
        let now_s = (kevy_store::now_unix_ms() / 1000) as i64;
        let mut narrow: Vec<IdxAdvice> = Vec::new();
        let mut unused: Vec<IdxAdvice> = Vec::new();
        let tables = self.tables.catalog.read().unwrap_or_else(PoisonError::into_inner);
        for (name, c) in self.indexes.usage.read().unwrap_or_else(PoisonError::into_inner).iter() {
            let margin = c.min_margin.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(dot) = name.iter().position(|&b| b == b'.')
                && let Some(spec) = tables.get(&name[..dot])
                && let Some(advice) = kevy_index::narrow_advice(spec, margin)
            {
                narrow.push(IdxAdvice { count: 0, name: name.clone(), advice });
            }
            let (hits, _, declared) = c.read();
            if hits == 0 {
                let n = String::from_utf8_lossy(name).into_owned();
                let age = (now_s - declared).max(0);
                unused.push(IdxAdvice {
                    count: 0,
                    name: name.clone(),
                    advice: format!("IDX.DROP {n}  (never hit in the {age}s since declare)"),
                });
            }
        }
        narrow.sort_by(|a, b| a.name.cmp(&b.name));
        unused.sort_by(|a, b| a.name.cmp(&b.name));
        narrow.extend(unused);
        narrow
    }

    /// Record one refused declaration family; past the threshold, on
    /// an opted-in table, the declare-period action runs right here —
    /// the refusal path is cold, and the query that pushed the count
    /// over still gets its error.
    pub(crate) fn observe_refused(&self, name: &[u8], shape: AdviseShape) {
        let count = self.tables.advise.lock().unwrap_or_else(PoisonError::into_inner).observe(
            name,
            shape.clone(),
            &[],
        );
        if count >= kevy_index::AUTODECLARE_AFTER {
            self.auto_declare(name, shape, count);
        }
    }

    /// The embedded declare-period action — same shared rule
    /// ([`kevy_index::apply_auto`]), same delta discipline as the
    /// server: a whole new path registers, a changed one (auto
    /// VALUES) rebuilds. Failures leave everything unchanged.
    fn auto_declare(&self, name: &[u8], shape: AdviseShape, count: u64) {
        let Some(dot) = name.iter().position(|&b| b == b'.') else { return };
        let mut spec = {
            let g = self.tables.catalog.read().unwrap_or_else(PoisonError::into_inner);
            match g.get(&name[..dot]) {
                Some(s) if s.autodeclare != 0 => s.clone(),
                _ => return,
            }
        };
        let entry =
            kevy_index::AdviseEntry { name: name.to_vec(), shape, count, sample: Vec::new() };
        let Some(ledger) = kevy_index::apply_auto(&mut spec, &entry) else { return };
        let Ok(compiled) = kevy_index::compile_table(&spec) else { return };
        let path = match ledger.iter().position(|&b| b == b'#') {
            Some(p) => ledger[..p].to_vec(),
            None => ledger,
        };
        let Some(ispec) = compiled.into_iter().find(|s| s.name == path) else { return };
        // Registry first, catalog second: once the name is free the
        // only register refusal is the tier floor, probed up front so
        // a refusal leaves the old path standing.
        #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
        if crate::ops_index_sync::tier_floor_check(&self.shards).is_err() {
            return;
        }
        self.idx_drop(&path);
        if self.register_spec(ispec).is_err() {
            return;
        }
        {
            let mut g = self.tables.catalog.write().unwrap_or_else(PoisonError::into_inner);
            g.drop_table(&spec.name);
            if g.create(spec).is_err() {
                return;
            }
        }
        self.persist_table_sidecar();
    }

    /// [`Self::observe_refused`] when `r` is a no-such-index refusal
    /// (the scalar walk's wording or the text face's) — the wrap for
    /// entry points whose refusal is born deep in the segment walk.
    pub(crate) fn observe_noindex<T>(&self, name: &[u8], shape: AdviseShape, r: &KevyResult<T>) {
        if let Err(KevyError::NotFound(m)) = r
            && (m == "no such index" || m == "no such text index")
        {
            self.observe_refused(name, shape);
        }
    }

    /// Forget every observed refusal (a catalog just changed).
    pub(crate) fn advise_clear(&self) {
        self.tables.advise.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// [`Self::idx_match_with`], additionally counting the values of
    /// the `FACET` fields over the whole match set — not just the
    /// page, which is why the counts cannot be derived from the hits.
    /// Lives here so the observation wrap sits with its family.
    #[cfg(feature = "text")]
    pub fn idx_match_faceted(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
        opts: crate::MatchOpts<'_>,
    ) -> KevyResult<crate::MatchPage> {
        let r = self.match_faceted_run(name, query, limit, opts);
        self.observe_noindex(name, AdviseShape::Match, &r);
        if r.is_ok() {
            self.observe_hit(name);
        }
        r
    }

    /// Count one served query against a declared path — the
    /// observation's dual, called by the same entry-point wraps.
    pub(crate) fn observe_hit(&self, name: &[u8]) {
        let cell =
            self.indexes.usage.read().unwrap_or_else(PoisonError::into_inner).get(name).cloned();
        if let Some(c) = cell {
            c.hit((kevy_store::now_unix_ms() / 1000) as i64);
        }
    }

    /// The usage cell for a declared path (None = not declared).
    /// Only the probe wraps read it, and the window tier compiles
    /// out on wasm.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn usage_cell(&self, name: &[u8]) -> Option<std::sync::Arc<kevy_index::UsageCell>> {
        self.indexes.usage.read().unwrap_or_else(PoisonError::into_inner).get(name).cloned()
    }

    /// Is `name` a path the auto loop declared (any table's ledger)?
    pub(crate) fn is_auto_path(&self, name: &[u8]) -> bool {
        let g = self.tables.catalog.read().unwrap_or_else(PoisonError::into_inner);
        g.iter().any(|s| s.auto_added.iter().any(|e| e == name))
    }

    /// `(hits, last_hit_s, declared_s)` for a declared path.
    #[must_use]
    pub fn idx_usage(&self, name: &[u8]) -> Option<(u64, i64, i64)> {
        self.indexes
            .usage
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|c| c.read())
    }

    /// Re-key the usage table to the current catalog, keeping
    /// same-name cells — counters survive unrelated installs,
    /// dropped paths drop, new paths date from now.
    pub(crate) fn usage_rekey(&self) {
        let names: Vec<Vec<u8>> = {
            let g = self.indexes.catalog.read().unwrap_or_else(PoisonError::into_inner);
            g.1.iter().map(|(s, _)| s.name.clone()).collect()
        };
        let now_s = (kevy_store::now_unix_ms() / 1000) as i64;
        let mut g = self.indexes.usage.write().unwrap_or_else(PoisonError::into_inner);
        let old = std::mem::take(&mut *g);
        for n in names {
            let cell = old
                .get(&n)
                .cloned()
                .unwrap_or_else(|| std::sync::Arc::new(kevy_index::UsageCell::declared_at(now_s)));
            g.insert(n, cell);
        }
    }
}
