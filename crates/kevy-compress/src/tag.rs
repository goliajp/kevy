//! Frame tags and the offset bound: the first byte of every frame says
//! which history the payload was encoded against.

/// Frame tag: payload is the original bytes verbatim.
///
/// # Examples
///
/// ```
/// use kevy_compress::{TAG_RAW, decode, encode};
///
/// // Too short for LZ to save a byte: stored as-is, never expanded past
/// // the header.
/// let frame = encode(b"", b"short");
/// assert_eq!(frame[0], TAG_RAW);
/// assert_eq!(decode(b"", &frame).unwrap(), b"short");
/// ```
pub const TAG_RAW: u8 = 0;
/// Frame tag: LZ token stream, history is the output alone.
///
/// # Examples
///
/// ```
/// use kevy_compress::{TAG_LZ, decode, encode};
///
/// let v = b"user:1 name=ada user:1 name=ada user:1 name=ada";
/// let frame = encode(b"", v);
/// assert_eq!(frame[0], TAG_LZ); // the repeats were found within the value
/// assert_eq!(decode(b"", &frame).unwrap(), v);
/// ```
pub const TAG_LZ: u8 = 1;
/// Frame tag: LZ token stream, history is `dict ++ output`.
///
/// # Examples
///
/// ```
/// use kevy_compress::{TAG_LZ_DICT, decode, encode, train};
///
/// let corpus: Vec<&[u8]> = vec![
///     b"level=info svc=api dur=12 msg=request handled cleanly",
///     b"level=warn svc=api dur=48 msg=request handled cleanly",
/// ];
/// let dict = train(&corpus, 4096);
/// let v = b"level=info svc=api dur=12 msg=request handled cleanly";
/// let frame = encode(&dict, v);
/// assert_eq!(frame[0], TAG_LZ_DICT); // it reached back into the dictionary
/// assert_eq!(decode(&dict, &frame).unwrap(), v);
/// // so it cannot be decoded without the dictionary
/// assert!(decode(b"", &frame).is_err());
/// ```
pub const TAG_LZ_DICT: u8 = 2;
/// Frame tag: high (compaction) level — literals Huffman-coded as one
/// block, byte-aligned sequence stream after it.
///
/// # Examples
///
/// ```
/// use kevy_compress::{TAG_LZH, decode, encode_high};
///
/// // Two repeats for the matcher, then a skewed tail that only an
/// // entropy coder can shrink.
/// let mut v = b"level=info svc=api msg=ok ".repeat(2);
/// let mut x: u32 = 999;
/// for _ in 0..1500 {
///     x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
///     v.push(b'a' + ((x >> 16).trailing_zeros() as u8).min(25));
/// }
/// let frame = encode_high(b"", &v);
/// assert_eq!(frame[0], TAG_LZH);
/// assert_eq!(decode(b"", &frame).unwrap(), v);
/// ```
pub const TAG_LZH: u8 = 3;
/// Frame tag: high level with dictionary history.
///
/// # Examples
///
/// ```
/// use kevy_compress::{MAX_OFFSET, TAG_LZH_DICT, decode, encode_high, train};
///
/// let corpus: Vec<&[u8]> = vec![
///     b"level=info svc=api dur=12 msg=request handled cleanly",
///     b"level=warn svc=api dur=48 msg=request handled cleanly",
/// ];
/// let dict = train(&corpus, MAX_OFFSET);
/// // A prefix the dictionary covers, then literals worth entropy-coding.
/// let mut v = b"level=info svc=api dur=12 msg=".to_vec();
/// let alphabet = b"aaaaaaaabbbbccde";
/// let mut x: u32 = 12345;
/// for _ in 0..400 {
///     x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
///     v.push(alphabet[(x >> 16) as usize % alphabet.len()]);
/// }
/// let frame = encode_high(&dict, &v);
/// assert_eq!(frame[0], TAG_LZH_DICT);
/// assert_eq!(decode(&dict, &frame).unwrap(), v);
/// ```
pub const TAG_LZH_DICT: u8 = 4;

/// Longest back-reference the 16-bit offset can express, which also
/// bounds how much trailing dictionary is reachable.
///
/// # Examples
///
/// ```
/// use kevy_compress::{MAX_OFFSET, train};
///
/// assert_eq!(MAX_OFFSET, 65_535);
/// // Passing it as the training budget asks for all the content a frame
/// // can reach; a larger budget buys no more of it, only room for the
/// // entropy table that sits above the reachable content.
/// let big = vec![b'x'; 200_000];
/// let samples: Vec<&[u8]> = vec![&big];
/// let at_limit = train(&samples, MAX_OFFSET);
/// assert!(at_limit.len() <= MAX_OFFSET);
/// assert!(train(&samples, 10 * MAX_OFFSET).len() - at_limit.len() < 256);
/// ```
pub const MAX_OFFSET: usize = u16::MAX as usize;
