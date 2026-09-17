//! `--replica`: act as a replica and print every command the server
//! replicates, as CSV.

use super::sync::{Payload, replconf, start, transfer};
use crate::rcli::format::{Output, render};
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};

/// Run until the server lets go; exit 1 then.
pub(crate) fn run(s: &mut Session) -> u8 {
    if let Err(msg) = follow(s) {
        eprint_bytes(&[&msg, b"\n"]);
    }
    1
}

fn follow(s: &mut Session) -> Result<(), Vec<u8>> {
    let _ = replconf(s, b"capa", b"eof"); // a server without it sends a sized payload
    let _ = replconf(s, b"rdb-filter-only", b""); // no keys wanted, only commands
    let payload = start(s)?;
    match &payload {
        Payload::UntilMark(_) => eprint_bytes(&[
            b"Full resync with master, discarding bytes of bulk transfer until EOF marker...\n",
        ]),
        Payload::Length(n) => eprint_bytes(&[format!(
            "Full resync with master, discarding {n} bytes of bulk transfer...\n"
        )
        .as_bytes()]),
    }
    let total = transfer(s, &payload, &mut |_| Ok(()))?;
    if let Payload::UntilMark(_) = payload {
        eprint_bytes(&[format!(
            "Full resync done after {total} bytes. Logging commands from master.\n"
        )
        .as_bytes()]);
        // A diskless master waits for an ACK before it streams commands.
        std::thread::sleep(std::time::Duration::from_secs(1));
        eprint_bytes(&[b"sending REPLCONF ACK 0\n"]);
        let conn = s.conn.as_mut().ok_or_else(|| b"Error: not connected".to_vec())?;
        conn.send(&[b"REPLCONF".to_vec(), b"ACK".to_vec(), b"0".to_vec()])
            .map_err(|e| [b"Error: ".as_slice(), e.text().as_bytes()].concat())?;
    } else {
        eprint_bytes(&[b"Full resync done. Logging commands from master.\n"]);
    }
    let delims = s.opts.delims.clone();
    loop {
        let conn = s.conn.as_mut().ok_or_else(|| b"Error: not connected".to_vec())?;
        let (reply, texts) =
            conn.read_reply().map_err(|e| [b"Error: ".as_slice(), e.text().as_bytes()].concat())?;
        write_out(&render(&reply, &texts, Output::Csv, &delims, false));
    }
}
