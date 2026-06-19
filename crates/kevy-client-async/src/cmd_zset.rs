//! Async mirror of sorted-set commands on `kevy_client::Connection`.

use std::io;

use kevy_resp::Reply;

use crate::cmd_set::set_multi;
use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected, vec2, vec3};

impl AsyncConnection {
    /// `ZADD key score member [score member ...]`. Returns count of
    /// newly added (overwrites don't count).
    pub async fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(2 + pairs.len() * 2);
        args.push(b"ZADD".to_vec());
        args.push(key.to_vec());
        for (score, m) in pairs {
            args.push(score.to_string().into_bytes());
            args.push(m.to_vec());
        }
        match self.codec_mut().request(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `ZREM key member [member ...]`. Returns count actually removed.
    pub async fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.codec_mut(), b"ZREM", key, members).await
    }

    /// `ZSCORE key member`. `None` if absent.
    pub async fn zscore(&mut self, key: &[u8], member: &[u8]) -> io::Result<Option<f64>> {
        match self
            .codec_mut()
            .request(&vec3(b"ZSCORE", key, member))
            .await?
        {
            Reply::Bulk(v) => {
                let s = std::str::from_utf8(&v)
                    .map_err(|_| io::Error::other("non-utf8 score reply"))?;
                let n: f64 = s
                    .parse()
                    .map_err(|_| io::Error::other(format!("bad score float: {s}")))?;
                Ok(Some(n))
            }
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `ZCARD key`. 0 if absent.
    pub async fn zcard(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request(&vec2(b"ZCARD", key)).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `ZRANGE key start stop`. Ascending-score order; negative indices
    /// count from the tail.
    pub async fn zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> io::Result<Vec<Vec<u8>>> {
        let args = vec![
            b"ZRANGE".to_vec(),
            key.to_vec(),
            start.to_string().into_bytes(),
            stop.to_string().into_bytes(),
        ];
        match self.codec_mut().request(&args).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
