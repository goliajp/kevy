//! `Store::xadd`: one entry appended, its fields copied once into it.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use super::{MissingStream, StreamId, XAddIdSpec};
use crate::value::SmallBytes;
use crate::{Store, StoreError};

impl Store {
    /// `XADD key <spec> field value [field value ...]`. Returns the
    /// assigned ID; with [`MissingStream::Refuse`] (`NOMKSTREAM`) a
    /// missing key stays missing and the answer is `Ok(None)`. `now_ms`
    /// is the wall-clock used for `XAddIdSpec::AutoAll`.
    pub fn xadd(
        &mut self,
        key: &[u8],
        spec: XAddIdSpec,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        missing: MissingStream,
        now_ms: u64,
    ) -> Result<Option<StreamId>, StoreError> {
        let borrowed = fields.iter().map(|(f, v)| (f.as_slice(), v.as_slice()));
        self.xadd_from(key, spec, borrowed, missing, now_ms)
    }

    /// [`Self::xadd`] from borrowed field-value pairs, copied once, into
    /// the entry.
    ///
    /// ```
    /// use kevy_store::{MissingStream, StreamId, XAddIdSpec};
    /// let mut s = kevy_store::Store::new();
    /// let spec = XAddIdSpec::Explicit(StreamId::new(1, 1));
    /// let id = s.xadd_from(b"s", spec, [(&b"f"[..], &b"v"[..])].into_iter(), MissingStream::Create, 0)?;
    /// assert_eq!(id, Some(StreamId::new(1, 1)));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xadd_from<'f>(
        &mut self,
        key: &[u8],
        spec: XAddIdSpec,
        fields: impl ExactSizeIterator<Item = (&'f [u8], &'f [u8])>,
        missing: MissingStream,
        now_ms: u64,
    ) -> Result<Option<StreamId>, StoreError> {
        if missing == MissingStream::Refuse && self.live_entry(key).is_none() {
            return Ok(None);
        }
        let id;
        let weight_delta;
        {
            let s = self.stream_mut(key, true)?.expect("created");
            id = s.resolve_xadd_id(spec, now_ms)?;
            let smb_fields: Vec<(SmallBytes, SmallBytes)> = fields
                .map(|(f, v)| (SmallBytes::from_slice(f), SmallBytes::from_slice(v)))
                .collect();
            weight_delta = super::stream_entry_weight(&smb_fields);
            s.insert(id, smb_fields);
        }
        self.bump_if_watched(key);
        self.account_delta(key, weight_delta as i64);
        Ok(Some(id))
    }
}
