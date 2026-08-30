//! Hash field-TTL: `HEXPIRE` / `HPEXPIRE` / `HPERSIST` / `HTTL`
//! (Redis 7.4 command shape).
//!
//! Per-field replies come back in request order. The `HEXPIRE` /
//! `HPEXPIRE` result codes ([`HExpireCode`], re-exported from
//! kevy-embedded): `-2` key or field missing, `0` condition not met,
//! `1` deadline set, `2` field deleted (deadline already due).

use crate::{KevyError, KevyResult};

use kevy_embedded::{HExpireCode, HExpireCond};
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, string, unexpected};

impl Connection {
    /// `HEXPIRE key seconds [NX|XX|GT|LT] FIELDS n field…` — set
    /// per-field TTLs, **whole-second** precision (`ttl` is truncated
    /// to seconds on both backends, matching the verb; use
    /// [`Self::hpexpire`] for milliseconds). One code per field.
    pub fn hexpire(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        ttl: std::time::Duration,
        cond: HExpireCond,
    ) -> KevyResult<Vec<HExpireCode>> {
        check_fields(fields)?;
        let secs = ttl.as_secs();
        match self {
            Self::Embedded(s) => s.hexpire(key, fields, std::time::Duration::from_secs(secs), cond),
            Self::Remote(c) => {
                hash_ttl_request(c, b"HEXPIRE", key, Some(secs.to_string()), cond, fields)
                    .map(to_codes)
            }
        }
    }

    /// `HPEXPIRE key milliseconds [NX|XX|GT|LT] FIELDS n field…` —
    /// millisecond-precision variant of [`Self::hexpire`].
    pub fn hpexpire(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        ttl: std::time::Duration,
        cond: HExpireCond,
    ) -> KevyResult<Vec<HExpireCode>> {
        check_fields(fields)?;
        let ms = ttl.as_millis().min(i64::MAX as u128) as u64;
        match self {
            Self::Embedded(s) => s.hexpire(key, fields, std::time::Duration::from_millis(ms), cond),
            Self::Remote(c) => {
                hash_ttl_request(c, b"HPEXPIRE", key, Some(ms.to_string()), cond, fields)
                    .map(to_codes)
            }
        }
    }

    /// `HPERSIST key FIELDS n field…` — clear per-field TTLs. Codes:
    /// `-2` missing, `-1` had no TTL, `1` cleared.
    pub fn hpersist(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<HExpireCode>> {
        check_fields(fields)?;
        match self {
            Self::Embedded(s) => s.hpersist(key, fields),
            Self::Remote(c) => {
                hash_ttl_request(c, b"HPERSIST", key, None, HExpireCond::Always, fields)
                    .map(to_codes)
            }
        }
    }

    /// `HTTL key FIELDS n field…` — remaining TTL per field in **seconds**
    /// (`-2` key/field missing, `-1` no TTL).
    ///
    /// Before 4.0 this returned milliseconds, which no caller reading the name
    /// would have expected. [`Self::hpttl`] is the millisecond form.
    pub fn httl(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<i64>> {
        check_fields(fields)?;
        match self {
            Self::Embedded(s) => s.httl(key, fields),
            Self::Remote(c) => hash_ttl_request(c, b"HTTL", key, None, HExpireCond::Always, fields),
        }
    }

    /// `HPTTL key FIELDS n field…` — remaining TTL per field in
    /// **milliseconds** (`-2` key/field missing, `-1` no TTL).
    pub fn hpttl(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<i64>> {
        check_fields(fields)?;
        match self {
            Self::Embedded(s) => s.hpttl(key, fields),
            Self::Remote(c) => {
                hash_ttl_request(c, b"HPTTL", key, None, HExpireCond::Always, fields)
            }
        }
    }
}

/// The verbs require `numfields > 0` — surface that before the wire.
fn check_fields(fields: &[&[u8]]) -> KevyResult<()> {
    if fields.is_empty() {
        return Err(KevyError::InvalidInput("hash field-TTL verbs need at least one field".into()));
    }
    Ok(())
}

fn cond_keyword(cond: HExpireCond) -> Option<&'static [u8]> {
    match cond {
        HExpireCond::Always => None,
        HExpireCond::Nx => Some(b"NX"),
        HExpireCond::Xx => Some(b"XX"),
        HExpireCond::Gt => Some(b"GT"),
        HExpireCond::Lt => Some(b"LT"),
    }
}

/// Build `verb key [arg] [cond] FIELDS n field…`, parse the per-field
/// integer array.
fn hash_ttl_request(
    c: &mut RespClient,
    verb: &[u8],
    key: &[u8],
    arg: Option<String>,
    cond: HExpireCond,
    fields: &[&[u8]],
) -> KevyResult<Vec<i64>> {
    let mut args = Vec::with_capacity(fields.len() + 6);
    args.push(verb.to_vec());
    args.push(key.to_vec());
    if let Some(a) = arg {
        args.push(a.into_bytes());
    }
    if let Some(kw) = cond_keyword(cond) {
        args.push(kw.to_vec());
    }
    args.push(b"FIELDS".to_vec());
    args.push(fields.len().to_string().into_bytes());
    args.extend(fields.iter().map(|f| f.to_vec()));
    match c.request(&args)? {
        Reply::Array(items) => items
            .into_iter()
            .map(|r| match r {
                Reply::Int(n) => Ok(n),
                other => Err(unexpected(other)),
            })
            .collect(),
        Reply::Error(e) => Err(KevyError::Protocol(string(e))),
        other => Err(unexpected(other)),
    }
}

fn to_codes(v: Vec<i64>) -> Vec<HExpireCode> {
    v.into_iter().map(|n| n as HExpireCode).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn embedded_hexpire_httl_hpersist_round_trip() {
        let mut c = Connection::connect("mem://").unwrap();
        c.hset(b"h", &[(b"a".as_ref(), b"1".as_ref()), (b"b".as_ref(), b"2".as_ref())]).unwrap();

        let codes = c
            .hexpire(b"h", &[&b"a"[..], &b"nope"[..]], Duration::from_secs(60), HExpireCond::Always)
            .unwrap();
        assert_eq!(codes, vec![1, -2]);

        let ttls = c.httl(b"h", &[&b"a"[..], &b"b"[..]]).unwrap();
        assert!((0..=60_000).contains(&ttls[0]), "ttl = {}", ttls[0]);
        assert_eq!(ttls[1], -1);

        let codes = c.hpersist(b"h", &[&b"a"[..], &b"b"[..]]).unwrap();
        assert_eq!(codes, vec![1, -1]);
        assert_eq!(c.httl(b"h", &[&b"a"[..]]).unwrap(), vec![-1]);
    }

    #[test]
    fn embedded_hpexpire_ms_precision() {
        let mut c = Connection::connect("mem://").unwrap();
        c.hset(b"h", &[(b"f".as_ref(), b"v".as_ref())]).unwrap();
        let codes = c
            .hpexpire(b"h", &[&b"f"[..]], Duration::from_millis(1500), HExpireCond::Always)
            .unwrap();
        assert_eq!(codes, vec![1]);
        let ttl = c.httl(b"h", &[&b"f"[..]]).unwrap()[0];
        assert!((0..=1500).contains(&ttl), "ttl = {ttl}");
    }

    #[test]
    fn empty_fields_rejected() {
        let mut c = Connection::connect("mem://").unwrap();
        let err = c.httl(b"h", &[]).unwrap_err();
        assert!(matches!(err, KevyError::InvalidInput(_)));
    }
}
