//! Where the log's value bytes went: the four terms of what compression
//! leaves behind, per file, summed over the files still on disk.

/// The compression accounting over every file the log still holds, dead
/// records included (the same population as [`crate::VlogStats::bytes`]).
///
/// `payload_bytes + frame_header_bytes` against `raw_bytes` is the ratio;
/// `payload_bytes` is the corpus's residual plus whatever redundancy the
/// match finder missed, which only a stronger encoder can split apart.
/// `dict_bytes` is memory, not disk: one dictionary per file.
/// # Examples
///
/// ```
/// use kevy_vlog::Vlog;
/// let dir = kevy_tmpdir::TmpDir::new("vlog-compression");
/// let mut v = Vlog::open(dir.path(), 1 << 20).unwrap();
/// v.append(b"k", &[b'a'; 400]).unwrap();
///
/// let c = v.compression();
/// assert_eq!(c.raw_bytes, 400);
/// // a run of one byte encodes to far less than it started as
/// assert!(c.payload_bytes + c.frame_header_bytes < 100);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompressionStats {
    /// Value bytes as the caller handed them in, before encoding.
    pub raw_bytes: u64,
    /// Encoded payload bytes on disk, frame headers excluded.
    pub payload_bytes: u64,
    /// Frame headers on disk: a tag byte and the LEB128 original length.
    pub frame_header_bytes: u64,
    /// Dictionary bytes held in memory, one dictionary per file.
    pub dict_bytes: u64,
}

impl CompressionStats {
    pub(crate) fn add_record(&mut self, raw_len: usize, frame_len: usize) {
        let header = 1 + leb128_len(raw_len);
        self.raw_bytes += raw_len as u64;
        self.frame_header_bytes += header as u64;
        self.payload_bytes += (frame_len - header) as u64;
    }

    pub(crate) fn sum(self, other: Self) -> Self {
        CompressionStats {
            raw_bytes: self.raw_bytes + other.raw_bytes,
            payload_bytes: self.payload_bytes + other.payload_bytes,
            frame_header_bytes: self.frame_header_bytes + other.frame_header_bytes,
            dict_bytes: self.dict_bytes + other.dict_bytes,
        }
    }
}

/// Bytes LEB128 spends on `n`: seven bits per byte.
fn leb128_len(n: usize) -> usize {
    let bits = usize::BITS - n.leading_zeros();
    (bits as usize).div_ceil(7).max(1)
}

#[cfg(test)]
mod tests {
    use super::leb128_len;

    #[test]
    fn leb128_length_steps_at_each_seven_bits() {
        for (n, len) in [(0, 1), (127, 1), (128, 2), (16_383, 2), (16_384, 3), (1 << 28, 5)] {
            assert_eq!(leb128_len(n), len, "{n}");
        }
    }
}
