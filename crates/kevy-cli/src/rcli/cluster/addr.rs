//! Node addresses on the command line: `host:port`, or `host port`.

use crate::rcli::cnum::atoi;
use crate::rcli::session::eprint_bytes;

/// A node to connect to, as the user named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Addr {
    pub(crate) host: Vec<u8>,
    pub(crate) port: i32,
}

impl Addr {
    /// `host:port`.
    pub(crate) fn shown(&self) -> Vec<u8> {
        [self.host.as_slice(), b":", self.port.to_string().as_bytes()].concat()
    }
}

/// `host:port` in one argument, or `host` and `port` in two; the port is
/// read leniently (`7000x` is 7000) and must not come out as 0.
pub(crate) fn entry(args: &[Vec<u8>]) -> Option<Addr> {
    let (host, port) = match args {
        [one] => {
            let colon = one.iter().rposition(|&b| b == b':')?;
            (one[..colon].to_vec(), atoi(&one[colon + 1..]))
        }
        [host, port] => (host.clone(), atoi(port)),
        _ => return None,
    };
    (port != 0).then_some(Addr { host, port })
}

/// Why an address argument was refused.
pub(crate) fn report_invalid() -> u8 {
    eprint_bytes(&[b"[ERR] Invalid arguments: you need to pass either a valid address (ie. 120.0.0.1:7000) or space separated IP and port (ie. 120.0.0.1 7000)\n"]);
    1
}

#[cfg(test)]
mod tests {
    use super::{Addr, entry};

    fn args(a: &[&str]) -> Vec<Vec<u8>> {
        a.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn an_address_is_one_argument_or_two() {
        let at = |h: &str, p| Some(Addr { host: h.into(), port: p });
        assert_eq!(entry(&args(&["127.0.0.1:7000"])), at("127.0.0.1", 7000));
        assert_eq!(entry(&args(&["::1:7000x"])), at("::1", 7000));
        assert_eq!(entry(&args(&["h", "-1"])), at("h", -1));
        assert_eq!(entry(&args(&[":7000"])), at("", 7000));
        for bad in [&["h"][..], &["h:x"], &["h", "0"], &["h", "1", "2"], &[]] {
            assert_eq!(entry(&args(bad)), None, "{bad:?}");
        }
    }
}
