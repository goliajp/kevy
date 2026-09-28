//! Refusing a port another server already listens on.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// `Err(AddrInUse)` when something already accepts connections on
/// `ip:port`; `Ok` otherwise.
///
/// Every shard binds with `SO_REUSEPORT`, which on its own lets a second
/// server started by the same user join the first one's listeners and take
/// a share of its connections. The claim this replaces opened a listener of
/// its own for a moment, and a client connecting in that moment was
/// accepted into it and reset when it closed. A connect asks exactly whether
/// anything listens, and holds nothing.
pub(crate) fn refuse_if_listened(ip: [u8; 4], port: u16) -> io::Result<()> {
    let host = if ip == [0, 0, 0, 0] { [127, 0, 0, 1] } else { ip };
    let addr = SocketAddr::from((host, port));
    // io_uring tears a dead process's ring down asynchronously, and its
    // listeners outlive the process by ~10 ms (measured 0-13 ms); a restart
    // inside that window must not read them as another server
    let deadline = Instant::now() + Duration::from_millis(200);
    while listened(&addr) {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("Address already in use: another server listens on {addr}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn listened(addr: &SocketAddr) -> bool {
    // a closed port refuses at once; the timeout only bounds a silent one
    TcpStream::connect_timeout(addr, Duration::from_millis(200)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::refuse_if_listened;

    #[test]
    fn a_listened_port_is_refused_and_a_free_one_is_not() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let err = refuse_if_listened([127, 0, 0, 1], port).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        drop(held);
        refuse_if_listened([127, 0, 0, 1], port).unwrap();
    }

    #[test]
    fn a_listener_that_goes_away_within_the_grace_is_not_a_holder() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let dying = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });
        refuse_if_listened([127, 0, 0, 1], port).unwrap();
        dying.join().unwrap();
    }
}
