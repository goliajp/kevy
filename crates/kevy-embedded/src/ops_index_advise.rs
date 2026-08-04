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
}
