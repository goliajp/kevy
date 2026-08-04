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
        // The reclaim face: declared paths no query has ever hit,
        // each with its age — dropping stays a human act.
        let now_s = (kevy_store::now_unix_ms() / 1000) as i64;
        let mut unused: Vec<IdxAdvice> = self
            .indexes
            .usage
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, c)| c.read().0 == 0)
            .map(|(name, c)| {
                let n = String::from_utf8_lossy(name).into_owned();
                let age = (now_s - c.read().2).max(0);
                IdxAdvice {
                    count: 0,
                    name: name.clone(),
                    advice: format!("IDX.DROP {n}  (never hit in the {age}s since declare)"),
                }
            })
            .collect();
        unused.sort_by(|a, b| a.name.cmp(&b.name));
        rows.append(&mut unused);
        rows
    }

    /// Record one refused declaration family.
    pub(crate) fn observe_refused(&self, name: &[u8], shape: AdviseShape) {
        self.tables
            .advise
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(name, shape, &[]);
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
