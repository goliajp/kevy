//! The read-only whitelist — each verb maps onto the
//! existing embedded Store API (which does its own shard locking and
//! cross-shard merging).

use crate::Store;

fn bulk(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
    out.extend_from_slice(b);
    out.extend_from_slice(b"\r\n");
}

fn opt_bulk(out: &mut Vec<u8>, v: Option<Vec<u8>>) {
    match v {
        Some(b) => bulk(out, &b),
        None => out.extend_from_slice(b"$-1\r\n"),
    }
}

fn int(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(format!(":{v}\r\n").as_bytes());
}

fn arr(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(format!("*{n}\r\n").as_bytes());
}

fn err(out: &mut Vec<u8>, msg: &str) {
    out.extend_from_slice(format!("-{msg}\r\n").as_bytes());
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.trim().parse().ok()
}

/// Dispatch one whitelisted request.
// fn-length exemption: pure data-driven verb match table — one flat
// arm per whitelisted verb; multi-step verbs live in cmd_* helpers.
// LOC-WAIVER: data-driven verb dispatch table — one reply-emitter arm per verb.
pub(crate) fn dispatch(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let up = verb.to_ascii_uppercase();
    match up.as_slice() {
        b"PING" => out.extend_from_slice(b"+PONG\r\n"),
        b"ECHO" if argv.len() == 2 => bulk(out, &argv[1]),
        b"GET" if argv.len() == 2 => match s.get(&argv[1]) {
            Ok(v) => opt_bulk(out, v),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"MGET" if argv.len() >= 2 => {
            arr(out, argv.len() - 1);
            for k in &argv[1..] {
                opt_bulk(out, s.get(k).ok().flatten());
            }
        }
        b"EXISTS" if argv.len() >= 2 => {
            let keys: Vec<&[u8]> = argv[1..].iter().map(Vec::as_slice).collect();
            int(out, s.exists(&keys).unwrap_or(0) as i64);
        }
        b"TYPE" if argv.len() == 2 => {
            let t = s.type_of(&argv[1]);
            out.extend_from_slice(format!("+{t}\r\n").as_bytes());
        }
        b"TTL" if argv.len() == 2 => int(out, s.ttl_secs(&argv[1])),
        b"PTTL" if argv.len() == 2 => int(out, s.ttl_ms(&argv[1])),
        b"DBSIZE" if argv.len() == 1 => int(out, s.dbsize() as i64),
        b"KEYS" if argv.len() == 2 => {
            let pat = if argv[1] == b"*" { None } else { Some(argv[1].as_slice()) };
            let keys = s.keys(pat, None);
            arr(out, keys.len());
            for k in keys {
                bulk(out, &k);
            }
        }
        b"SCAN" if argv.len() >= 2 => cmd_scan(s, argv, out),
        b"HGET" if argv.len() == 3 => match s.hget(&argv[1], &argv[2]) {
            Ok(v) => opt_bulk(out, v),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"HMGET" if argv.len() >= 3 => {
            let fields: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
            match s.hmget(&argv[1], &fields) {
                Ok(vals) => {
                    arr(out, vals.len());
                    for v in vals {
                        opt_bulk(out, v);
                    }
                }
                Err(e) => err(out, &format!("WRONGTYPE {e}")),
            }
        }
        b"HGETALL" if argv.len() == 2 => match s.hgetall(&argv[1]) {
            Ok(pairs) => {
                arr(out, pairs.len() * 2);
                for (f, v) in pairs {
                    bulk(out, &f);
                    bulk(out, &v);
                }
            }
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"HLEN" if argv.len() == 2 => match s.hlen(&argv[1]) {
            Ok(n) => int(out, n as i64),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"LRANGE" if argv.len() == 4 => match (parse_i64(&argv[2]), parse_i64(&argv[3])) {
            (Some(a), Some(b)) => match s.lrange(&argv[1], a, b) {
                Ok(items) => {
                    arr(out, items.len());
                    for i in items {
                        bulk(out, &i);
                    }
                }
                Err(e) => err(out, &format!("WRONGTYPE {e}")),
            },
            _ => err(out, "ERR value is not an integer"),
        },
        b"LLEN" if argv.len() == 2 => match s.llen(&argv[1]) {
            Ok(n) => int(out, n as i64),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"SMEMBERS" if argv.len() == 2 => match s.smembers(&argv[1]) {
            Ok(ms) => {
                arr(out, ms.len());
                for m in ms {
                    bulk(out, &m);
                }
            }
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"SCARD" if argv.len() == 2 => match s.scard(&argv[1]) {
            Ok(n) => int(out, n as i64),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"SISMEMBER" if argv.len() == 3 => match s.sismember(&argv[1], &argv[2]) {
            Ok(b) => int(out, i64::from(b)),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"ZSCORE" if argv.len() == 3 => match s.zscore(&argv[1], &argv[2]) {
            Ok(Some(sc)) => bulk(out, format!("{sc}").as_bytes()),
            Ok(None) => out.extend_from_slice(b"$-1\r\n"),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"ZCARD" if argv.len() == 2 => match s.zcard(&argv[1]) {
            Ok(n) => int(out, n as i64),
            Err(e) => err(out, &format!("WRONGTYPE {e}")),
        },
        b"ZRANGE" if argv.len() >= 4 => cmd_zrange(s, argv, out),
        #[cfg(feature = "replicate")]
        b"FEED.SHARDS" if argv.len() == 1 => int(out, s.feed_shards() as i64),
        #[cfg(feature = "replicate")]
        b"FEED.TAIL" if argv.len() == 1 => match s.changes_tail() {
            Ok((g, o)) => {
                arr(out, 2);
                int(out, g as i64);
                int(out, o as i64);
            }
            Err(e) => err(out, &format!("ERR feed: {e:?}")),
        },
        #[cfg(feature = "replicate")]
        b"FEED.READ" if argv.len() >= 4 => cmd_feed_read(s, argv, out),
        b"INFO" => {
            let mut body = format!(
                "# kevy-embedded\r\nversion:{}\r\nshards:{}\r\nkeys:{}\r\nlistener:readonly\r\n",
                env!("CARGO_PKG_VERSION"),
                s.shard_count(),
                s.dbsize()
            );
            // `# Tiering`: present only when tiering is on
            // — an untiered store's INFO body stays byte-identical.
            if let Some(t) = s.tier_info() {
                body.push_str(&format!(
                    "\r\n# Tiering\r\ntiering_enabled:1\r\ntier_budget_bytes:{}\r\n\
                     tier_effective_target:{}\r\ncold_keys:{}\r\ncold_bytes:{}\r\n\
                     stub_bytes:{}\r\nindex_reserved_bytes:{}\r\nvlog_size_bytes:{}\r\n\
                     vlog_live_bytes:{}\r\nvlog_files:{}\r\nvlog_epoch:{}\r\n\
                     demotions_total:{}\r\npromotions_total:{}\r\n\
                     peek_preads_total:{}\r\nbatch_submissions_total:{}\r\n",
                    t.tier_budget_bytes,
                    t.tier_effective_target,
                    t.cold_keys,
                    t.cold_bytes,
                    t.stub_bytes,
                    t.index_reserved_bytes,
                    t.vlog_size_bytes,
                    t.vlog_live_bytes,
                    t.vlog_files,
                    t.vlog_epoch,
                    t.demotions_total,
                    t.promotions_total,
                    t.peek_preads_total,
                    t.batch_submissions_total,
                ));
            }
            bulk(out, body.as_bytes());
        }
        _ => err(out, "ERR READONLY embedded listener"),
    }
}

fn cmd_scan(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(cursor) = std::str::from_utf8(&argv[1]).ok().and_then(|c| c.parse::<u64>().ok())
    else {
        return err(out, "ERR invalid cursor");
    };
    let mut pattern: Option<&[u8]> = None;
    let mut count = 100usize;
    let mut i = 2;
    while i + 1 < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(&argv[i + 1]);
        } else if argv[i].eq_ignore_ascii_case(b"COUNT") {
            count = parse_i64(&argv[i + 1]).unwrap_or(100).clamp(1, 10_000) as usize;
        } else {
            return err(out, "ERR syntax error");
        }
        i += 2;
    }
    let (next, keys) = s.scan(cursor, pattern, count);
    arr(out, 2);
    bulk(out, next.to_string().as_bytes());
    arr(out, keys.len());
    for k in keys {
        bulk(out, &k);
    }
}

fn cmd_zrange(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let (Some(a), Some(b)) = (parse_i64(&argv[2]), parse_i64(&argv[3])) else {
        return err(out, "ERR value is not an integer");
    };
    let withscores = argv.len() == 5 && argv[4].eq_ignore_ascii_case(b"WITHSCORES");
    if argv.len() > 5 || (argv.len() == 5 && !withscores) {
        return err(out, "ERR syntax error");
    }
    match s.zrange(&argv[1], a, b) {
        Ok(items) => {
            arr(out, items.len() * if withscores { 2 } else { 1 });
            for (m, sc) in items {
                bulk(out, &m);
                if withscores {
                    bulk(out, format!("{sc}").as_bytes());
                }
            }
        }
        Err(e) => err(out, &format!("WRONGTYPE {e}")),
    }
}

#[cfg(feature = "replicate")]
fn cmd_feed_read(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let (Some(g), Some(o), Some(limit)) =
        (parse_i64(&argv[1]), parse_i64(&argv[2]), parse_i64(&argv[3]))
    else {
        return err(out, "ERR FEED.READ gen offset limit [PREFIX p…]");
    };
    let mut prefixes: Vec<&[u8]> = Vec::new();
    if argv.len() > 4 {
        if !argv[4].eq_ignore_ascii_case(b"PREFIX") || argv.len() < 6 {
            return err(out, "ERR FEED.READ gen offset limit [PREFIX p…]");
        }
        prefixes = argv[5..].iter().map(Vec::as_slice).collect();
    }
    match s.changes_since(g as u64, o as u64, limit.clamp(1, 10_000) as usize, &prefixes) {
        Ok(batch) => {
            arr(out, 3);
            int(out, batch.next.0 as i64);
            int(out, batch.next.1 as i64);
            arr(out, batch.changes.len());
            for f in &batch.changes {
                arr(out, f.argv.len());
                for a in &f.argv {
                    bulk(out, a);
                }
            }
        }
        Err(e) => err(out, &format!("ERR feed: {e:?}")),
    }
}
