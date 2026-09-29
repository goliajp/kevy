//! Reading acknowledged writes back from a recovered server.

// Teardown. `shutdown` on a socket the peer has closed reports what
// already happened, and nobody is left to tell.
#![expect(clippy::let_underscore_must_use, reason = "teardown has nobody left to report to")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::AckEntry;

/// Verify that every entry in `acks` is readable from kevy at `port`,
/// using a SINGLE pipelined TCP connection (one large batched
/// write, single drain read, parse replies in order). This avoids the
/// ephemeral-port exhaustion that ruined an earlier run of this suite,
/// where each GET opened a fresh TCP conn (Mac's ~16 k ephemeral
/// ports × 60 s TIME_WAIT capped sustainable rate at ~267 conns/s; the
/// chaos test verifies hundreds of thousands of ACKs per run).
///
/// Returns Ok(()) if all match or Err on first corrupted (wrong value)
/// or lost (nil reply) entry. Use [`pipelined_verify_counts`] when the
/// caller wants to count instead of fail-fast.
///
/// ```
/// # use std::io::{Read, Write};
/// # fn serve(reply: &'static [u8]) -> std::io::Result<u16> {
/// #     let l = std::net::TcpListener::bind("127.0.0.1:0")?;
/// #     let port = l.local_addr()?.port();
/// #     std::thread::spawn(move || {
/// #         let (mut s, _) = l.accept().expect("the verifier connects");
/// #         let _ = s.read_to_end(&mut Vec::new());
/// #         let _ = s.write_all(reply);
/// #     });
/// #     Ok(port)
/// # }
/// use kevy_chaos::AckEntry;
///
/// let acks = [
///     AckEntry { key: b"k0".to_vec(), value: b"v0".to_vec(), seq: 0 },
///     AckEntry { key: b"k1".to_vec(), value: b"v1".to_vec(), seq: 1 },
/// ];
/// // `serve` stands in for a recovered kevy: it answers the pipelined GETs
/// // with the given replies
///
/// let port = serve(b"$2\r\nv0\r\n$2\r\nv1\r\n")?;
/// assert_eq!(kevy_chaos::verify_all_present(port, &acks), Ok(()));
///
/// // k1 came back missing: the acknowledged write was lost
/// let port = serve(b"$2\r\nv0\r\n$-1\r\n")?;
/// let err = kevy_chaos::verify_all_present(port, &acks).unwrap_err();
/// assert!(err.starts_with("LOST 1 of 2"), "{err}");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn verify_all_present(port: u16, acks: &[AckEntry]) -> Result<(), String> {
    let (present, lost, corrupted) = pipelined_verify_counts(port, acks);
    if !corrupted.is_empty() {
        return Err(format!(
            "CORRUPTION DETECTED — {} keys returned wrong values:\n{}",
            corrupted.len(),
            corrupted.join("\n")
        ));
    }
    if lost > 0 {
        return Err(format!("LOST {lost} of {} ACKs (present {present})", acks.len()));
    }
    Ok(())
}

/// Same as `verify_all_present` but returns counts instead of fail-fast.
/// Returns `(present, lost, corrupted_descriptions)`.
///
/// ```
/// # use std::io::{Read, Write};
/// # fn serve(reply: &'static [u8]) -> std::io::Result<u16> {
/// #     let l = std::net::TcpListener::bind("127.0.0.1:0")?;
/// #     let port = l.local_addr()?.port();
/// #     std::thread::spawn(move || {
/// #         let (mut s, _) = l.accept().expect("the verifier connects");
/// #         let _ = s.read_to_end(&mut Vec::new());
/// #         let _ = s.write_all(reply);
/// #     });
/// #     Ok(port)
/// # }
/// use kevy_chaos::AckEntry;
///
/// let acks = [
///     AckEntry { key: b"k0".to_vec(), value: b"v0".to_vec(), seq: 0 },
///     AckEntry { key: b"k1".to_vec(), value: b"v1".to_vec(), seq: 1 },
/// ];
/// // `serve` stands in for a recovered kevy: it answers the pipelined GETs
/// // with the given replies
///
/// // k0 is intact, k1 came back with a different value
/// let port = serve(b"$2\r\nv0\r\n$2\r\nxx\r\n")?;
/// let (present, lost, corrupted) = kevy_chaos::pipelined_verify_counts(port, &acks);
/// assert_eq!((present, lost), (1, 0));
/// assert_eq!(corrupted.len(), 1);
/// assert!(corrupted[0].contains(r#"expected="v1" got="xx""#), "{}", corrupted[0]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn pipelined_verify_counts(port: u16, acks: &[AckEntry]) -> (usize, usize, Vec<String>) {
    let buf = match pipeline_get_replies(port, acks) {
        Ok(buf) => buf,
        Err(e) => return (0, acks.len(), vec![e]),
    };
    tally_replies(acks, &buf)
}

/// Send one pipelined GET per ACK entry and drain the whole reply
/// stream into a single buffer. Errors carry a human description.
fn pipeline_get_replies(port: u16, acks: &[AckEntry]) -> Result<Vec<u8>, String> {
    let mut s = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(e) => return Err(format!("connect: {e}")),
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    // Send-all then drain-all keeps the pipeline simple. The sender
    // thread half-closes after writing so the read side sees EOF.
    let mut send_buf = Vec::with_capacity(acks.len() * 32);
    for ack in acks {
        send_buf.extend_from_slice(b"*2\r\n$3\r\nGET\r\n");
        send_buf.extend_from_slice(format!("${}\r\n", ack.key.len()).as_bytes());
        send_buf.extend_from_slice(&ack.key);
        send_buf.extend_from_slice(b"\r\n");
    }
    let send_handle = std::thread::spawn(move || {
        s.write_all(&send_buf).map_err(|e| format!("pipeline write: {e}"))?;
        let _ = s.shutdown(std::net::Shutdown::Write);
        Ok::<_, String>(s)
    });
    let mut s = match send_handle.join() {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("sender thread panicked".into()),
    };
    let mut buf = Vec::with_capacity(8 * 1024 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    Ok(buf)
}

/// Parse replies in order matching the ACK log. Returns
/// `(present, lost, corrupted_descriptions)`.
fn tally_replies(acks: &[AckEntry], buf: &[u8]) -> (usize, usize, Vec<String>) {
    let mut present = 0usize;
    let mut lost = 0usize;
    let mut corrupted = Vec::new();
    let mut pos = 0usize;
    for ack in acks {
        match parse_one_reply(buf, pos) {
            Some((Some(val), next)) => {
                if val == ack.value {
                    present += 1;
                } else {
                    corrupted.push(format!(
                        "key={:?} expected={:?} got={:?}",
                        String::from_utf8_lossy(&ack.key),
                        String::from_utf8_lossy(&ack.value),
                        String::from_utf8_lossy(val),
                    ));
                }
                pos = next;
            }
            Some((None, next)) => {
                lost += 1;
                pos = next;
            }
            None => {
                lost += 1;
            }
        }
    }
    (present, lost, corrupted)
}

fn parse_one_reply(buf: &[u8], start: usize) -> Option<(Option<&[u8]>, usize)> {
    let rest = &buf[start..];
    if rest.starts_with(b"$-1\r\n") {
        return Some((None, start + 5));
    }
    if rest.first() != Some(&b'$') {
        return None;
    }
    let nl = rest.iter().position(|&b| b == b'\n')?;
    let len_str = std::str::from_utf8(&rest[1..nl - 1]).ok()?;
    let len: usize = len_str.parse().ok()?;
    let body_start = nl + 1;
    let body_end = body_start + len;
    if rest.len() < body_end + 2 {
        return None;
    }
    Some((Some(&rest[body_start..body_end]), start + body_end + 2))
}
