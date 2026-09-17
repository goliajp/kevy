//! `--rdb <file>` / `--functions-rdb <file>`: a snapshot of the server,
//! fetched over the replication protocol, into a file (`-` for stdout).

use super::sync::{Payload, replconf, start, transfer};
use crate::rcli::session::{Session, eprint_bytes};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;

/// Fetch and write; exit 0 on success, 1 with the reason otherwise.
pub(crate) fn run(s: &mut Session, functions_only: bool) -> u8 {
    match fetch(s, functions_only) {
        Ok(()) => 0,
        Err(msg) => {
            eprint_bytes(&[&msg, b"\n"]);
            1
        }
    }
}

fn fetch(s: &mut Session, functions_only: bool) -> Result<(), Vec<u8>> {
    let name = s.opts.modes.rdb_file.clone().unwrap_or_default();
    let _ = replconf(s, b"capa", b"eof"); // a server without it sends a sized payload
    let _ = replconf(s, b"rdb-only", b"1"); // an older server sends a full sync, still an RDB first
    if functions_only && replconf(s, b"rdb-filter-only", b"functions").is_err() {
        return Err(b"Failed requesting functions only RDB from server, aborting".to_vec());
    }
    let payload = start(s)?;
    match &payload {
        Payload::UntilMark(_) => eprint_bytes(&[
            b"SYNC sent to master, writing bytes of bulk transfer until EOF marker to '",
            &name,
            b"'\n",
        ]),
        Payload::Length(n) => eprint_bytes(&[
            format!("SYNC sent to master, writing {n} bytes to '").as_bytes(),
            &name,
            b"'\n",
        ]),
    }
    let mut out = Output::open(&name)?;
    let total = transfer(s, &payload, &mut |bytes| out.write(bytes))?;
    match payload {
        Payload::UntilMark(_) => {
            eprint_bytes(&[
                format!("Transfer finished with success after {total} bytes\n").as_bytes()
            ])
        }
        Payload::Length(_) => eprint_bytes(&[b"Transfer finished with success.\n"]),
    }
    s.conn = None;
    out.finish(&name, total)
}

/// Where the snapshot goes.
enum Output {
    Stdout,
    File(std::fs::File),
}

impl Output {
    /// Created 0644 if missing; not truncated until the length is known.
    fn open(name: &[u8]) -> Result<Output, Vec<u8>> {
        if name == b"-" {
            return Ok(Output::Stdout);
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            // Kept, then cut to the snapshot's length at the end, as redis-cli does.
            .truncate(false)
            .mode(0o644)
            .open(std::ffi::OsStr::from_bytes(name))
            .map(Output::File)
            .map_err(|e| {
                [
                    b"Error opening '".as_slice(),
                    name,
                    b"': ",
                    crate::rcli::conn::strerror(&e).as_bytes(),
                ]
                .concat()
            })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Vec<u8>> {
        let written = match self {
            Output::Stdout => std::io::stdout().lock().write_all(bytes),
            Output::File(f) => f.write_all(bytes),
        };
        written.map_err(|e| {
            [b"Error writing data to file: ".as_slice(), crate::rcli::conn::strerror(&e).as_bytes()]
                .concat()
        })
    }

    /// Cut an existing longer file to the snapshot, and make it durable.
    fn finish(self, name: &[u8], length: u64) -> Result<(), Vec<u8>> {
        let Output::File(file) = self else {
            let _ = std::io::stdout().flush(); // stdout has no fsync to fail
            return Ok(());
        };
        let fail = |e: std::io::Error| {
            [
                b"Fail to fsync '".as_slice(),
                name,
                b"': ",
                crate::rcli::conn::strerror(&e).as_bytes(),
            ]
            .concat()
        };
        file.set_len(length).map_err(fail)?;
        file.sync_all().map_err(fail)
    }
}
