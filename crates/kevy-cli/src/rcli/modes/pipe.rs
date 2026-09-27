//! `--pipe`: send standard input to the server as it is (RESP or inline
//! commands) and count the replies, for mass insertion.
//!
//! One thread copies stdin to the socket and ends with an ECHO of 20 random
//! bytes; this thread reads replies until that echo comes back, so it knows
//! the last reply without counting commands it never parsed.

use super::random::Random;
use crate::rcli::conn::LinkError;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What the reply loop saw.
#[derive(Default)]
struct Tally {
    replies: u64,
    errors: u64,
}

/// Transfer, wait for the last reply, report; exit 1 when a reply was an
/// error or the wait timed out.
pub(crate) fn run(s: &mut Session) -> u8 {
    let Some(conn) = s.conn.as_mut() else { return 1 };
    let mut random = Random::seeded();
    let mark: Vec<u8> = (0..20).map(|_| random.next_u64() as u8).collect();
    let sent_all = Arc::new(AtomicBool::new(false));
    let writer = match conn.writer() {
        Ok(w) => w,
        Err(e) => {
            eprint_bytes(&[
                b"Error writing to the server: ",
                crate::rcli::conn::strerror(&e).as_bytes(),
                b"\n",
            ]);
            return 1;
        }
    };
    let copier = {
        let (mark, sent_all) = (mark.clone(), Arc::clone(&sent_all));
        std::thread::spawn(move || copy_stdin(writer, &mark, &sent_all))
    };
    // A bounded read lets the loop notice a timeout once all is sent.
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200))); // without it, --pipe-timeout cannot fire
    let timeout = u64::try_from(s.opts.modes.pipe_timeout).unwrap_or(0);
    let tally = read_replies(conn, &mark, &sent_all, timeout);
    drop(copier); // the copier exits on its own at end of input or a write error
    write_out(format!("errors: {}, replies: {}\n", tally.errors, tally.replies).as_bytes());
    u8::from(tally.errors > 0)
}

/// Copy stdin to the server, then the closing ECHO.
fn copy_stdin(mut to: Box<dyn Write + Send>, mark: &[u8], sent_all: &AtomicBool) {
    let mut from = std::io::stdin().lock();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprint_bytes(&[
                    b"Error reading from stdin: ",
                    crate::rcli::conn::strerror(&e).as_bytes(),
                    b"\n",
                ]);
                std::process::exit(1);
            }
        };
        if let Err(e) = to.write_all(&chunk[..n]) {
            eprint_bytes(&[
                b"Error writing to the server: ",
                crate::rcli::conn::strerror(&e).as_bytes(),
                b"\n",
            ]);
            std::process::exit(1);
        }
    }
    // Said before the ECHO goes out, so its reply cannot be reported first.
    write_out(b"All data transferred. Waiting for the last reply...\n");
    let echo = [b"\r\n*2\r\n$4\r\nECHO\r\n$20\r\n".as_slice(), mark, b"\r\n"].concat();
    if let Err(e) = to.write_all(&echo) {
        eprint_bytes(&[
            b"Error writing to the server: ",
            crate::rcli::conn::strerror(&e).as_bytes(),
            b"\n",
        ]);
        std::process::exit(1);
    }
    sent_all.store(true, Ordering::Relaxed);
}

/// Replies until the ECHO of `mark`, or `timeout` seconds without one after
/// all input was sent (0: wait for ever).
fn read_replies(
    conn: &mut crate::rcli::conn::Conn,
    mark: &[u8],
    sent_all: &AtomicBool,
    timeout: u64,
) -> Tally {
    let mut tally = Tally::default();
    let mut last_reply = Instant::now();
    loop {
        match conn.read_reply() {
            Ok((Reply::Bulk(echo), _)) if echo == mark => {
                write_out(b"Last reply received from server.\n");
                return tally;
            }
            Ok((reply, _)) => {
                last_reply = Instant::now();
                tally.replies += 1;
                if let Reply::Error(msg) | Reply::BlobError(msg) = reply {
                    tally.errors += 1;
                    eprint_bytes(&[&msg, b"\n"]);
                }
            }
            Err(LinkError::Io(
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut,
                _,
            )) => {
                if timeout > 0
                    && sent_all.load(Ordering::Relaxed)
                    && last_reply.elapsed() > Duration::from_secs(timeout)
                {
                    tally.errors += 1;
                    eprint_bytes(&[
                        format!("No replies for {timeout} seconds: exiting.\n").as_bytes()
                    ]);
                    return tally;
                }
            }
            Err(_) => {
                eprint_bytes(&[b"Error reading replies from server\n"]);
                std::process::exit(1);
            }
        }
    }
}
