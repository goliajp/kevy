//! The parsed-reply type returned by [`crate::parse_reply`].

/// A parsed RESP reply (server → client) — the client-side counterpart of
/// the crate's `encode_*` functions (server-side encoders).
///
/// Variants prefixed with `Resp3:` in their doc are only ever produced by
/// a server speaking RESP3; an `HELLO 2` (or no `HELLO`) session sees the
/// RESP2 subset (`Simple` / `Error` / `Int` / `Bulk` / `Nil` / `Array`)
/// exclusively. The variant set is RESP3's type set and deliberately
/// exhaustive: a `match` on `Reply` lists every wire type, so a decoder
/// cannot silently drop one.
///
/// ```
/// use kevy_resp::{Reply, parse_reply};
///
/// let (reply, used) = parse_reply(b"*2\r\n$3\r\nfoo\r\n:7\r\n")?.expect("complete frame");
/// assert_eq!(used, 17);
/// match reply {
///     Reply::Array(items) => assert_eq!(items, [Reply::Bulk(b"foo".to_vec()), Reply::Int(7)]),
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), kevy_resp::ProtocolError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `+OK`
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// assert_eq!(parse_reply(b"+OK\r\n")?, Some((Reply::Simple(b"OK".to_vec()), 5)));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Simple(Vec<u8>),
    /// `-ERR ...`
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"-ERR no such key\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::Error(b"ERR no such key".to_vec()));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Error(Vec<u8>),
    /// `:42`
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// assert_eq!(parse_reply(b":-3\r\n")?, Some((Reply::Int(-3), 5)));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Int(i64),
    /// `$5\r\nhello\r\n`
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"$5\r\nhello\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::Bulk(b"hello".to_vec()));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Bulk(Vec<u8>),
    /// `$-1` or `*-1` — the RESP2 null sentinel; in RESP3 the dedicated
    /// [`Reply::Null`] (`_\r\n`) is used instead. Both round-trip here.
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// assert_eq!(parse_reply(b"$-1\r\n")?, Some((Reply::Nil, 5)));
    /// assert_eq!(parse_reply(b"*-1\r\n")?, Some((Reply::Nil, 5)));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Nil,
    /// `*N ...`
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"*2\r\n:1\r\n+x\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::Array(vec![Reply::Int(1), Reply::Simple(b"x".to_vec())]));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Array(Vec<Reply>),
    /// **Resp3:** `%N\r\n<key1><value1>...<keyN><valueN>` — N pairs (the
    /// header count is the pair count, NOT the element count, so a map of
    /// 3 pairs is `%3` plus 6 sub-replies). Parsed/exposed as a Vec of
    /// pairs so duplicate keys + insertion order are preserved.
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"%1\r\n+f\r\n:1\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::Map(vec![(Reply::Simple(b"f".to_vec()), Reply::Int(1))]));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Map(Vec<(Reply, Reply)>),
    /// **Resp3:** `~N\r\n<item1>...<itemN>` — set semantics on the wire;
    /// dedup is the application's job (RESP3 doesn't require it).
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"~2\r\n:1\r\n:1\r\n")?.expect("complete frame");
    /// // duplicates reach the caller as sent
    /// assert_eq!(reply, Reply::Set(vec![Reply::Int(1), Reply::Int(1)]));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Set(Vec<Reply>),
    /// **Resp3:** `,1.5\r\n` — double. `inf` / `-inf` / `nan` are valid
    /// payloads per the RESP3 spec and survive the round-trip.
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// assert_eq!(parse_reply(b",1.5\r\n")?, Some((Reply::Double(1.5), 6)));
    /// let (inf, _) = parse_reply(b",inf\r\n")?.expect("complete frame");
    /// assert_eq!(inf, Reply::Double(f64::INFINITY));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Double(f64),
    /// **Resp3:** `#t\r\n` / `#f\r\n` — boolean.
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// assert_eq!(parse_reply(b"#t\r\n")?, Some((Reply::Boolean(true), 4)));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Boolean(bool),
    /// **Resp3:** `=15\r\ntxt:Some bytes\r\n` — verbatim string carrying
    /// a 3-char format tag (`txt` / `mkd` / etc.) + raw bytes. The colon
    /// separator is part of the wire encoding but not part of `data`.
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"=9\r\ntxt:hello\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::Verbatim { fmt: *b"txt", data: b"hello".to_vec() });
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Verbatim {
        /// 3-char format tag (e.g. `b"txt"` for plain text, `b"mkd"` for markdown).
        ///
        /// ```
        /// use kevy_resp::{Reply, parse_reply};
        /// let (reply, _) = parse_reply(b"=8\r\nmkd:# hi\r\n")?.expect("complete frame");
        /// let Reply::Verbatim { fmt, .. } = reply else { panic!("not verbatim") };
        /// assert_eq!(&fmt, b"mkd");
        /// # Ok::<(), kevy_resp::ProtocolError>(())
        /// ```
        fmt: [u8; 3],
        /// Payload bytes following the `:` separator.
        ///
        /// ```
        /// use kevy_resp::{Reply, parse_reply};
        /// let (reply, _) = parse_reply(b"=6\r\ntxt:ab\r\n")?.expect("complete frame");
        /// let Reply::Verbatim { data, .. } = reply else { panic!("not verbatim") };
        /// assert_eq!(data, b"ab");
        /// # Ok::<(), kevy_resp::ProtocolError>(())
        /// ```
        data: Vec<u8>,
    },
    /// **Resp3:** `(170141183460469231731687303715884105727\r\n` — arbitrary-
    /// precision integer; carried as the raw digit bytes since we don't
    /// pull in a bignum crate (charter: zero deps).
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"(12345678901234567890123\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::BigNumber(b"12345678901234567890123".to_vec()));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    BigNumber(Vec<u8>),
    /// **Resp3:** `_\r\n` — true null. RESP2 falls back to [`Reply::Nil`].
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// assert_eq!(parse_reply(b"_\r\n")?, Some((Reply::Null, 3)));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Null,
    /// **Resp3:** `>N\r\n...` — like [`Reply::Array`] but tagged as an
    /// out-of-band server-push frame (pub/sub messages in RESP3). The
    /// client must dispatch these separately from regular replies.
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b">2\r\n+message\r\n+hi\r\n")?.expect("complete frame");
    /// assert!(matches!(reply, Reply::Push(ref parts) if parts.len() == 2));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    Push(Vec<Reply>),
    /// **Resp3:** `!8\r\nERR ohno\r\n` — error carried as a length-prefixed
    /// bulk (handles errors containing CRLF that the simple-string `-`
    /// shape can't encode).
    ///
    /// ```
    /// use kevy_resp::{Reply, parse_reply};
    /// let (reply, _) = parse_reply(b"!8\r\nERR ohno\r\n")?.expect("complete frame");
    /// assert_eq!(reply, Reply::BlobError(b"ERR ohno".to_vec()));
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    BlobError(Vec<u8>),
}
