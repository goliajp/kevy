//! A request that outlives the connection: the modes that watch a server
//! over time (`--stat`, `--latency`, …) reconnect and carry on.

use crate::rcli::conn::LinkError;
use crate::rcli::send::write_out;
use crate::rcli::session::{Connect, Session, eprint_bytes};
use kevy_resp::Reply;
use std::time::Duration;

impl Session {
    /// `argv`'s reply, reconnecting once a second for as long as the server
    /// is gone, and saying so on the current line. A protocol error, which a
    /// reconnect would not cure, ends the program.
    pub(crate) fn request_reconnecting(&mut self, argv: &[&[u8]]) -> Reply {
        let mut tries = 0u64;
        loop {
            match self.request(argv) {
                Ok(reply) => {
                    if tries > 0 {
                        write_out(b"\r\x1b[0K");
                    }
                    return reply;
                }
                Err(e @ LinkError::Protocol(_)) => {
                    eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
                    std::process::exit(1);
                }
                Err(_) => {
                    tries += 1;
                    write_out(format!("\r\x1b[0KReconnecting... {tries}\r").as_bytes());
                    self.connect(Connect::Quiet);
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}
