//! `--stat`: a line of the server's vital signs every interval, from INFO.

use super::human::bytes;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;
use std::time::Duration;

const HEADER: &[u8] =
    b"------- data ------ --------------------- load -------------------- - child -\n\
keys       mem      clients blocked requests            connections          \n";

/// Print rows until the program is stopped; 1 if INFO fails.
pub(crate) fn run(s: &mut Session) -> u8 {
    let databases = database_count(s);
    let interval = if s.opts.interval_us == 0 { 1_000_000 } else { s.opts.interval_us };
    let mut previous: Option<i64> = None;
    for row in 0u64.. {
        let info = match s.request_reconnecting(&[b"INFO"]) {
            Reply::Bulk(text) | Reply::Verbatim { data: text, .. } => text,
            Reply::Error(msg) => {
                eprint_bytes(&[b"ERROR: ", &msg, b"\n"]);
                return 1;
            }
            _ => Vec::new(),
        };
        if row.is_multiple_of(20) {
            write_out(HEADER);
        }
        let requests = field(&info, b"total_commands_processed");
        write_out(&line(&info, databases, requests, previous));
        previous = Some(requests);
        std::thread::sleep(Duration::from_micros(interval));
    }
    0
}

/// `CONFIG GET databases`; on an error, 16 and a note saying so.
fn database_count(s: &mut Session) -> i64 {
    match s.request(&[b"CONFIG", b"GET", b"databases"]) {
        Ok(Reply::Array(pair)) if pair.len() == 2 => match &pair[1] {
            Reply::Bulk(n) => crate::rcli::cnum::atoll(n),
            _ => 16,
        },
        Ok(Reply::Error(msg)) => {
            eprint_bytes(&[
                b"CONFIG GET databases fails: ",
                &msg,
                b", use default value 16 instead\n",
            ]);
            16
        }
        Ok(_) => 16,
        Err(_) => {
            eprint_bytes(&[b"\nI/O error\n"]);
            std::process::exit(1);
        }
    }
}

/// One row: keys, memory, clients, blocked, requests (+since last), total
/// connections, and a child process at work, if one is.
fn line(info: &[u8], databases: i64, requests: i64, previous: Option<i64>) -> Vec<u8> {
    let keys: i64 = (0..databases).map(|n| keyspace_keys(info, n)).sum();
    let delta = previous.map_or(0, |p| requests - p);
    let child = field(info, b"rdb_bgsave_in_progress")
        | field(info, b"aof_rewrite_in_progress") << 1
        | field(info, b"loading") << 2;
    let child = match child {
        1 => "SAVE",
        2 => "AOF",
        3 => "SAVE+AOF",
        4 => "LOAD",
        _ => "",
    };
    format!(
        "{keys:<11}{:<8} {:<8}{:<8}{:<19} {:<12}{child}\n",
        bytes(field(info, b"used_memory").max(0) as u64),
        field(info, b"connected_clients"),
        field(info, b"blocked_clients"),
        format!("{requests} (+{delta})"),
        field(info, b"total_connections_received"),
    )
    .into_bytes()
}

/// An `INFO` field as a number; 0 when absent.
fn field(info: &[u8], name: &[u8]) -> i64 {
    info.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(b":"))
        .map_or(0, crate::rcli::cnum::atoll)
}

/// `keys=` of the `db<n>:` line; 0 when the database is empty.
fn keyspace_keys(info: &[u8], n: i64) -> i64 {
    let label = format!("db{n}:");
    info.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(label.as_bytes()))
        .and_then(|rest| rest.split(|&b| b == b',').find_map(|kv| kv.strip_prefix(b"keys=")))
        .map_or(0, crate::rcli::cnum::atoll)
}

#[cfg(test)]
mod tests {
    use super::{field, keyspace_keys, line};

    const INFO: &[u8] = b"# Server\r\nused_memory:1548288\r\nused_memory_human:1.48M\r\nconnected_clients:1\r\n\
blocked_clients:0\r\ntotal_commands_processed:13\r\ntotal_connections_received:4\r\nrdb_bgsave_in_progress:0\r\n\
aof_rewrite_in_progress:1\r\nloading:0\r\n# Keyspace\r\ndb0:keys=2,expires=0,avg_ttl=0\r\ndb1:keys=1,expires=0\r\ndb16:keys=9\r\n";

    #[test]
    fn a_row_from_info() {
        assert_eq!(field(INFO, b"used_memory"), 1_548_288, "not used_memory_human");
        assert_eq!(field(INFO, b"missing"), 0);
        assert_eq!(keyspace_keys(INFO, 1), 1);
        let row = String::from_utf8_lossy(&line(INFO, 16, 13, None)).into_owned();
        assert_eq!(
            row, "3          1.48M    1       0       13 (+0)             4           AOF\n",
            "db16 is past 16 databases"
        );
        let next = String::from_utf8_lossy(&line(INFO, 16, 15, Some(13))).into_owned();
        assert!(next.contains("15 (+2)"), "{next}");
    }
}
