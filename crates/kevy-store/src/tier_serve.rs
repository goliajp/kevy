//! The tiering READ funnel + the no-promote peek surface.
//!
//! Three read lanes come through here:
//!
//! - **Client reads** (`tier_serve`): the promotion gate — the first
//!   materializing access serves decoded bytes without installing
//!   (probation `touched` mark), the second promotes.
//! - **Bulk whole-value reads** ([`Store::peek_scope`]): digest,
//!   scope-move, exports. Inside the scope every cold materializing
//!   read serves via pread WITHOUT setting the mark and WITHOUT
//!   promoting — a bulk sweep is not an access signal.
//! - **Row-field reads** ([`Store::peek_hash_fields`] /
//!   [`Store::peek_hash_rows`]): hydration and index backfill. ONE
//!   record read + ONE decode per cold ROW extracts every requested
//!   field (never one pread per field); a page's cold rows coalesce —
//!   sorted by `(file_id, offset)` — into one [`ColdBatchReader`]
//!   batch (io_uring: one submit-and-wait for N READ SQEs; poller /
//!   embedded: an ordered `read_at` loop).
//!
//! Counter contract (asserted by the D1/D3 gates): `peek_preads_total`
//! += 1 per cold ROW peeked; `batch_submissions_total` += the reader's
//! kernel submission count per page with ≥1 cold row.

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod enabled {
    use std::sync::Arc;

    use kevy_vlog::{VlogFile, VlogRef, verify_image};

    use crate::value::{COLD_TAG_HASH, ColdRef, Value};
    use crate::{Entry, Store, StoreError};

    /// One planned cold-record read in a [`Store::peek_hash_rows`]
    /// batch. The pinned file keeps the record readable even if a
    /// compaction retires the file mid-batch.
    pub struct ColdRead {
        /// Pinned vlog file the record lives in.
        pub file: Arc<VlogFile>,
        /// Record address; the image to fetch is `vref.disk_len()`
        /// bytes at `vref.offset`.
        pub vref: VlogRef,
    }

    /// The read-issuance half of a cold batch: fetch every record
    /// image, in `reads` order. [`SyncColdRead`] is the ordered
    /// positional-read loop (poller reactors + embedded); the server's
    /// io_uring backend submits the batch to a secondary ring instead.
    pub trait ColdBatchReader {
        /// Fetch each `reads[i]`'s raw image (`vref.disk_len()` bytes
        /// at `vref.offset`, unverified — the store runs
        /// [`verify_image`] + decode on completion). Returns the images
        /// plus the number of kernel submissions made (1 for the sync
        /// loop, ceil(n / ring entries) for a ring).
        fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)>;
    }

    /// The default reader: one ordered `pread` per record.
    pub struct SyncColdRead;

    impl ColdBatchReader for SyncColdRead {
        fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
            let mut images = Vec::with_capacity(reads.len());
            for r in reads {
                images.push(r.file.read_image(r.vref)?);
            }
            Ok((images, 1))
        }
    }

    /// One peeked row: the per-field values of a live hash
    /// (`Ok(Some(..))`, one `Option` per requested field), a missing
    /// key (`Ok(None)`), or a non-hash (`Err(WrongType)`).
    pub type PeekRow = Result<Option<Vec<Option<Vec<u8>>>>, StoreError>;

    /// Stage-1 verdict for one peeked key (zero IO — the stub's tag
    /// answers WRONGTYPE without a pread).
    enum Probe {
        Missing,
        WrongType,
        Hot,
        ColdHash(ColdRef),
    }

    /// Extract `fields` from a live hot entry, hmget-shaped. `Err` on a
    /// non-hash; `Cold` never reaches here (callers resolve it first).
    fn hot_hash_fields(
        e: &Entry,
        fields: &[&[u8]],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        match &e.value {
            Value::Hash(h) => Ok(fields.iter().map(|f| h.get(*f).cloned()).collect()),
            Value::SmallHashInline(h) => {
                Ok(fields.iter().map(|f| h.get(f).map(<[u8]>::to_vec)).collect())
            }
            _ => Err(StoreError::WrongType),
        }
    }

    impl Store {
        /// Stage-2 funnel for WRITE paths: a live Cold entry whose tag
        /// matches `want` is promoted in place; a mismatch is WRONGTYPE
        /// with zero preads. Hot values / absent keys pass through.
        pub(crate) fn tier_resolve(&mut self, key: &[u8], want: u8) -> Result<(), StoreError> {
            if !self.cold_backing {
                return Ok(());
            }
            let tag = match self.live_entry(key) {
                Some(Entry { value: Value::Cold(c), .. }) => c.type_tag,
                _ => return Ok(()),
            };
            if tag != want {
                return Err(StoreError::WrongType);
            }
            self.promote_in_place(key);
            Ok(())
        }

        /// Stage-2 funnel for READ paths — `live_entry` plus the
        /// promotion gate. On a live Cold entry with a matching tag:
        /// first materializing access decodes into the serve scratch
        /// (no install, probation mark set); the second promotes. A tag
        /// mismatch is WRONGTYPE with zero preads. Hot/absent =
        /// `live_entry` verbatim. Inside [`Store::peek_scope`] the gate
        /// is bypassed: serve via scratch, mark untouched, no promote.
        pub(crate) fn tier_serve(&mut self, key: &[u8], want: u8) -> Result<Option<&Entry>, StoreError> {
            if !self.cold_backing {
                return Ok(self.live_entry(key));
            }
            let cold = match self.live_entry(key) {
                None => return Ok(None),
                Some(e) => match &e.value {
                    Value::Cold(c) => Some((c.type_tag, c.touched != 0)),
                    _ => None,
                },
            };
            match cold {
                None => Ok(self.live_entry(key)),
                Some((tag, _)) if tag != want => Err(StoreError::WrongType),
                Some(_) if self.tier_peek => self.tier_serve_cold(key, true),
                Some((_, true)) => {
                    self.promote_in_place(key);
                    Ok(self.live_entry(key))
                }
                Some((_, false)) => self.tier_serve_cold(key, false),
            }
        }

        /// Cold serve without installing: decode the record into the
        /// scratch entry and hand a reference to it. The scratch
        /// mirrors the live entry's TTL so callers that read
        /// `expire_at_ns` behave identically. `peek = false` is the
        /// gate's first touch (probation mark set); `peek = true` is a
        /// bulk read (mark untouched, peek pread counted).
        fn tier_serve_cold(&mut self, key: &[u8], peek: bool) -> Result<Option<&Entry>, StoreError> {
            let (cref, expire) = {
                let e = self.map.get_mut(key).expect("probed live above");
                let Value::Cold(c) = &mut e.value else { unreachable!("cold checked above") };
                if !peek {
                    c.touched = 1;
                }
                (*c, e.expire_at_ns)
            };
            let value = self.tier_read_record(key, cref);
            if peek
                && !cref.is_seg()
                && let Some(t) = self.tier.as_mut()
            {
                t.peek_preads_total += 1;
            }
            let mut entry = Entry::new(value, None);
            entry.expire_at_ns = expire;
            entry.set_weight(u64::from(cref.weight));
            self.tier_scratch = Some(entry);
            Ok(self.tier_scratch.as_ref())
        }

        /// Read + decode one cold record (bumps the pread counter). A
        /// vlog read/decode failure is a process bug by the vlog's
        /// per-boot doctrine — surfaced loudly, never healed silently.
        pub(crate) fn tier_read_record(&mut self, key: &[u8], cref: ColdRef) -> Value {
            if cref.is_seg() {
                return self.segrow_read(cref, key);
            }
            let t = self.tier.as_mut().expect("tier enabled");
            t.preads_total += 1;
            let (_key, payload) = t
                .vlog
                .read(cref.vref())
                .expect("tier: vlog read failed — per-boot spill file, this is a process bug");
            crate::tier_codec::decode(cref.type_tag, payload)
                .expect("tier: cold record decode failed — process bug")
        }

        /// `&self` peek for the zero-copy shared lane and COPY: decode
        /// a fresh owned value from the record WITHOUT installing,
        /// promoting, or setting the probation mark (documented: the
        /// shared lane pays a pread until a `&mut`-path access
        /// promotes). `None` when the value is not Cold. (Counter-free:
        /// the shared lane is `&self`; the `&mut` peeks carry the
        /// counters the gates assert on.)
        pub(crate) fn tier_peek_value(&self, key: &[u8], v: &Value) -> Option<Value> {
            let Value::Cold(c) = v else { return None };
            if c.is_seg() {
                return Some(self.segrow_read(*c, key));
            }
            let t = self.tier.as_ref().expect("cold value ⇒ tiering on");
            let (_key, payload) = t
                .vlog
                .read(c.vref())
                .expect("tier: vlog read failed — per-boot spill file, this is a process bug");
            Some(
                crate::tier_codec::decode(c.type_tag, payload)
                    .expect("tier: cold record decode failed — process bug"),
            )
        }

        /// Run `f` in bulk-read (no-promote peek) mode: every cold
        /// materializing read inside serves via pread WITHOUT setting
        /// the probation mark and WITHOUT promoting. The whole-value
        /// peek for digest / scope-move / export sweeps — a bulk
        /// sweep must never thrash the hot tier.
        pub fn peek_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
            let prev = self.tier_peek;
            self.tier_peek = true;
            let r = f(self);
            self.tier_peek = prev;
            r
        }

        /// Row peek: `fields` of the hash at `key`, without
        /// promotion and without advancing the touched gate. A hot row
        /// reads as `hmget` does; a COLD hash stub costs ONE record
        /// read + ONE decode for all fields. `Ok(None)` = missing key;
        /// `Err(WrongType)` = non-hash (zero preads when cold — the
        /// stage-1 tag answers).
        pub fn peek_hash_fields(
            &mut self,
            key: &[u8],
            fields: &[&[u8]],
        ) -> PeekRow {
            self.purge_hash_ttl(key);
            match self.peek_probe(key) {
                Probe::Missing => Ok(None),
                Probe::WrongType => Err(StoreError::WrongType),
                Probe::Hot => {
                    let e = self.live_entry(key).expect("probed live above");
                    Ok(Some(hot_hash_fields(e, fields)?))
                }
                Probe::ColdHash(cref) => {
                    let value = self.tier_read_record(key, cref);
                    if !cref.is_seg()
                        && let Some(t) = self.tier.as_mut()
                    {
                        t.peek_preads_total += 1;
                    }
                    let Value::Hash(h) = &value else {
                        unreachable!("hash-tagged record decodes to a hash")
                    };
                    Ok(Some(fields.iter().map(|f| h.get(*f).cloned()).collect()))
                }
            }
        }

        /// The seg-backed half of a cold plan, served synchronously
        /// (one fence descent each — batching belongs to the vlog's
        /// pread model). Returns the vlog-backed remainder.
        fn serve_seg_rows(
            &mut self,
            out: &mut [PeekRow],
            plan: Vec<(usize, ColdRef)>,
            fields: &[&[u8]],
            keys: &[&[u8]],
        ) -> Vec<(usize, ColdRef)> {
            let mut kept = Vec::with_capacity(plan.len());
            for (row, c) in plan {
                if !c.is_seg() {
                    kept.push((row, c));
                    continue;
                }
                let value = self.segrow_read(c, keys[row]);
                let Value::Hash(h) = &value else {
                    unreachable!("hash-tagged record decodes to a hash")
                };
                out[row] = Ok(Some(fields.iter().map(|f| h.get(*f).cloned()).collect()));
            }
            kept
        }

        /// Stage-1 classification for the peeks — the same
        /// probe-then-act two-phase shape `tier_serve` uses (borrow
        /// split; the second `live_entry` on the hot arm mirrors it).
        fn peek_probe(&mut self, key: &[u8]) -> Probe {
            match self.live_entry(key) {
                None => Probe::Missing,
                Some(e) => match &e.value {
                    Value::Cold(c) if c.type_tag == COLD_TAG_HASH => Probe::ColdHash(*c),
                    Value::Cold(_) => Probe::WrongType,
                    Value::Hash(_) | Value::SmallHashInline(_) => Probe::Hot,
                    _ => Probe::WrongType,
                },
            }
        }

        /// Page peek: `keys` × `fields` with every cold row
        /// coalesced — sorted by `(file_id, offset)` — into ONE
        /// [`ColdBatchReader`] batch, decoded once per row, results in
        /// input order. Per-key result mirrors
        /// [`Store::peek_hash_fields`]. No promotion, no gate
        /// advancement; hot rows read exactly as `hmget` does.
        pub fn peek_hash_rows(
            &mut self,
            keys: &[&[u8]],
            fields: &[&[u8]],
            reader: &mut dyn ColdBatchReader,
        ) -> Vec<PeekRow> {
            let mut out = Vec::with_capacity(keys.len());
            let mut plan: Vec<(usize, ColdRef)> = Vec::new();
            for (i, key) in keys.iter().enumerate() {
                self.purge_hash_ttl(key);
                match self.peek_probe(key) {
                    Probe::Missing => out.push(Ok(None)),
                    Probe::WrongType => out.push(Err(StoreError::WrongType)),
                    Probe::Hot => {
                        let e = self.live_entry(key).expect("probed live above");
                        out.push(hot_hash_fields(e, fields).map(Some));
                    }
                    Probe::ColdHash(cref) => {
                        plan.push((i, cref));
                        out.push(Ok(None)); // patched after the batch
                    }
                }
            }
            if !plan.is_empty() {
                self.peek_cold_batch(&mut out, plan, fields, reader, keys);
            }
            out
        }

        /// The cold half of [`Store::peek_hash_rows`]: sort, batch-read
        /// through `reader`, verify + decode each record ONCE, extract
        /// all fields, patch results back in original row order.
        fn peek_cold_batch(
            &mut self,
            out: &mut [PeekRow],
            plan: Vec<(usize, ColdRef)>,
            fields: &[&[u8]],
            reader: &mut dyn ColdBatchReader,
            keys: &[&[u8]],
        ) {
            let mut plan = self.serve_seg_rows(out, plan, fields, keys);
            if plan.is_empty() {
                return;
            }
            plan.sort_by_key(|(_, c)| (c.file_id, c.offset));
            let t = self.tier.as_mut().expect("cold value ⇒ tiering on");
            let reads: Vec<ColdRead> = plan
                .iter()
                .map(|(_, c)| ColdRead {
                    file: t.vlog.pin(c.file_id).expect("live stub names a live file"),
                    vref: c.vref(),
                })
                .collect();
            let (images, submissions) = reader
                .read_batch(&reads)
                .expect("tier: vlog batch read failed — per-boot spill file, this is a process bug");
            assert_eq!(images.len(), reads.len(), "reader must return one image per read");
            t.preads_total += plan.len() as u64;
            t.peek_preads_total += plan.len() as u64;
            t.batch_submissions_total += submissions;
            for ((row, cref), image) in plan.into_iter().zip(images) {
                let (_key, payload) = verify_image(cref.vref(), image)
                    .expect("tier: cold record image verify failed — process bug");
                let value = crate::tier_codec::decode(cref.type_tag, payload)
                    .expect("tier: cold record decode failed — process bug");
                let Value::Hash(h) = &value else {
                    unreachable!("hash-tagged record decodes to a hash")
                };
                out[row] = Ok(Some(fields.iter().map(|f| h.get(*f).cloned()).collect()));
            }
        }
    }
}

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use enabled::{ColdBatchReader, ColdRead, PeekRow, SyncColdRead};

/// Funnel/peek passthroughs for builds without the tier backend
/// (no_std / wasm): `Value::Cold` cannot be constructed there, so the
/// funnel degenerates to `live_entry` and the peeks to plain hot reads.
#[cfg(not(all(feature = "std", not(target_arch = "wasm32"))))]
mod disabled {
    use alloc::vec::Vec;

    use crate::value::Value;
    use crate::{Entry, Store, StoreError};

    impl Store {
        #[inline]
        pub(crate) fn tier_resolve(&mut self, _key: &[u8], _want: u8) -> Result<(), StoreError> {
            Ok(())
        }

        #[inline]
        pub(crate) fn tier_serve(&mut self, key: &[u8], _want: u8) -> Result<Option<&Entry>, StoreError> {
            Ok(self.live_entry(key))
        }

        #[inline]
        pub(crate) fn tier_peek_value(&self, _key: &[u8], _v: &Value) -> Option<Value> {
            None
        }

        /// No tier backend on this target — `f` runs with nothing to
        /// peek past.
        #[inline]
        pub fn peek_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
            f(self)
        }

        /// No tier backend on this target — the plain hot-row read.
        pub fn peek_hash_fields(
            &mut self,
            key: &[u8],
            fields: &[&[u8]],
        ) -> Result<Option<Vec<Option<Vec<u8>>>>, StoreError> {
            self.purge_hash_ttl(key);
            match self.live_entry(key) {
                None => Ok(None),
                Some(e) => match &e.value {
                    Value::Hash(h) => {
                        Ok(Some(fields.iter().map(|f| h.get(*f).cloned()).collect()))
                    }
                    Value::SmallHashInline(h) => {
                        Ok(Some(fields.iter().map(|f| h.get(f).map(<[u8]>::to_vec)).collect()))
                    }
                    _ => Err(StoreError::WrongType),
                },
            }
        }
    }
}
