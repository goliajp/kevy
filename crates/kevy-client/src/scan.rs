//! Key-space iteration: `KEYS`, `SCAN`, `RANDOMKEY`.
//!
//! Embedded backend uses `kevy_embedded::Store::with(|inner| ...)` to
//! reach `kevy_store::Store::collect_keys`. `SCAN` on embed is a single
//! shot — cursor always advances 0 → 0 (i.e., all keys returned at once).
//! Use `KEYS` directly if you want the same data; `SCAN` exists so the
//! same code can also run against a Redis server, where iteration matters.

use std::io;

use kevy_resp::Reply;

use crate::{Connection, array_to_bulks, string, unexpected};

impl Connection {
    /// `KEYS pattern` — every key matching `pattern` (glob: `*`, `?`,
    /// `[abc]`). Use sparingly: O(N) over the whole keyspace.
    pub fn keys(&mut self, pattern: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => Ok(s.with(|inner| inner.collect_keys(Some(pattern), None))),
            Self::Remote(c) => match c.request(&[b"KEYS".to_vec(), pattern.to_vec()])? {
                Reply::Array(items) => array_to_bulks(items),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `SCAN cursor [MATCH pattern] [COUNT n]`. Returns `(next_cursor,
    /// batch)`; iterate by re-calling with the returned cursor until
    /// `next_cursor == 0`.
    ///
    /// On the embedded backend the iteration always finishes in one
    /// call — `next_cursor` is `0` and `batch` holds every matching key.
    /// `count` is taken as a hint only.
    pub fn scan(
        &mut self,
        cursor: u64,
        pattern: Option<&[u8]>,
        count: Option<usize>,
    ) -> io::Result<(u64, Vec<Vec<u8>>)> {
        match self {
            Self::Embedded(s) => {
                // Embed has no real cursor: any non-zero cursor means the caller
                // already drained on a previous call.
                if cursor != 0 {
                    return Ok((0, Vec::new()));
                }
                let batch = s.with(|inner| inner.collect_keys(pattern, count));
                Ok((0, batch))
            }
            Self::Remote(c) => {
                let mut args: Vec<Vec<u8>> =
                    vec![b"SCAN".to_vec(), cursor.to_string().into_bytes()];
                if let Some(pat) = pattern {
                    args.push(b"MATCH".to_vec());
                    args.push(pat.to_vec());
                }
                if let Some(n) = count {
                    args.push(b"COUNT".to_vec());
                    args.push(n.to_string().into_bytes());
                }
                match c.request(&args)? {
                    Reply::Array(items) if items.len() == 2 => {
                        let mut it = items.into_iter();
                        let cursor_bulk = it.next().unwrap();
                        let keys_arr = it.next().unwrap();
                        let next_cursor = match cursor_bulk {
                            Reply::Bulk(b) => std::str::from_utf8(&b)
                                .map_err(|_| io::Error::other("non-utf8 SCAN cursor"))?
                                .parse()
                                .map_err(|_| io::Error::other("bad SCAN cursor"))?,
                            other => return Err(unexpected(other)),
                        };
                        let keys = match keys_arr {
                            Reply::Array(items) => array_to_bulks(items)?,
                            other => return Err(unexpected(other)),
                        };
                        Ok((next_cursor, keys))
                    }
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `RANDOMKEY` — sample one key, or `None` if the keyspace is empty.
    ///
    /// Embed returns the lexicographically-first key (deterministic, no
    /// RNG); the server returns a truly random key. Both honour empty.
    pub fn randomkey(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self {
            Self::Embedded(s) => Ok(s.with(|inner| {
                inner.collect_keys(None, Some(1)).into_iter().next()
            })),
            Self::Remote(c) => match c.request(&[b"RANDOMKEY".to_vec()])? {
                Reply::Bulk(v) => Ok(Some(v)),
                Reply::Nil => Ok(None),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_keys_matches_glob() {
        let mut c = Connection::open("mem://").unwrap();
        c.set(b"user:1", b"a").unwrap();
        c.set(b"user:2", b"b").unwrap();
        c.set(b"other", b"c").unwrap();
        let mut keys = c.keys(b"user:*").unwrap();
        keys.sort();
        assert_eq!(keys, vec![b"user:1".to_vec(), b"user:2".to_vec()]);
    }

    #[test]
    fn embedded_scan_returns_all_in_one_round() {
        let mut c = Connection::open("mem://").unwrap();
        for i in 0..5 {
            c.set(format!("k{i}").as_bytes(), b"v").unwrap();
        }
        let (next, batch) = c.scan(0, None, None).unwrap();
        assert_eq!(next, 0);
        assert_eq!(batch.len(), 5);
        // Any non-zero cursor means "we already finished" on embed.
        let (next2, batch2) = c.scan(123, None, None).unwrap();
        assert_eq!(next2, 0);
        assert!(batch2.is_empty());
    }

    #[test]
    fn embedded_randomkey_empty_and_present() {
        let mut c = Connection::open("mem://").unwrap();
        assert!(c.randomkey().unwrap().is_none());
        c.set(b"only", b"x").unwrap();
        assert_eq!(c.randomkey().unwrap(), Some(b"only".to_vec()));
    }
}
