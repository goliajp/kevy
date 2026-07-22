//! Text-index specifics for the embedded API: declaring a multi-field
//! text index, and gathering the corpus statistics a global BM25 scores
//! against. Split from `ops_index.rs` for the 500-LOC house rule (a
//! `#[path]` child module, so it reaches `Store`'s private fields).

use kevy_index::{IndexKind, IndexSpec, ValType};

use super::sync_segs;
use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

impl Store {
    /// A multi-field text index over several weighted hash fields — what
    /// the wire creates with `ON FIELDS f1 f2 WEIGHTS 2 1`, and what `IN`
    /// scopes to. A weight scales that field's term frequencies; 1.0 is
    /// neutral.
    ///
    /// `positions` records token offsets so phrase queries can verify
    /// adjacency, at the cost of the positional side-channel's memory.
    /// `values` names hash fields stored per document with the type their
    /// bytes compare as — what `FILTER` reads. An index that never
    /// filters declares none.
    pub fn idx_create_text(
        &self,
        name: &[u8],
        prefix: &[u8],
        fields: &[(&[u8], f32)],
        positions: bool,
        values: &[(&[u8], ValType)],
    ) -> KevyResult<()> {
        if prefix.is_empty() {
            return Err(KevyError::InvalidInput("empty prefix".into()));
        }
        if fields.is_empty() {
            return Err(KevyError::InvalidInput("a text index needs at least one field".into()));
        }
        let spec = IndexSpec {
            name: name.to_vec(),
            prefix: prefix.to_vec(),
            fields: fields
                .iter()
                .map(|(f, w)| kevy_index::FieldSpec { name: f.to_vec(), weight: *w })
                .collect(),
            ty: ValType::Str,
            kind: IndexKind::Text,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: positions,
            values: values
                .iter()
                .map(|(n, ty)| kevy_index::ValueSpec { name: n.to_vec(), ty: *ty })
                .collect(),
        };
        self.register_spec(spec)
    }

    /// Corpus-wide BM25 statistics for one query, over its field scope.
    ///
    /// Every shard reports its own document count, its token total and,
    /// per query term, its local document frequency; summing them is what
    /// makes the scores comparable across shards. Restricted to a field
    /// scope, each of those numbers describes those fields rather than
    /// whole documents, so a scoped query's `avgdl` is the average length
    /// *of the named fields*.
    pub(crate) fn text_corpus_stats_in(
        &self,
        name: &[u8],
        text: &[u8],
        typo: u32,
        scope: &[usize],
    ) -> KevyResult<kevy_text::CorpusStats> {
        let (mut n_docs, mut total_len) = (0f64, 0u64);
        // Accumulated per shard from `query_df_in`, which expands `word*`
        // prefixes against that shard's dictionary — so the df map ends up
        // keyed by the union of every shard's query terms and prefix
        // expansions, each summed to a global df.
        let mut df: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                found = true;
                n_docs += ts.docs() as f64;
                total_len += ts.total_len_in(scope);
                let opts = kevy_text::QueryOpts {
                    stats: None,
                    typo,
                    fields: scope,
                    filter: &[],
                    sort: None,
                    distinct: None,
                };
                for (t, d) in ts.query_df_in(text, opts) {
                    *df.entry(t).or_insert(0) += d;
                }
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such text index".into()));
        }
        let avgdl = if n_docs > 0.0 { total_len as f64 / n_docs } else { 0.0 };
        Ok(kevy_text::CorpusStats { n_docs, avgdl, df })
    }
}
