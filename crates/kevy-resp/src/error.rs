//! Protocol-level parsing error shared by request + reply parsers,
//! plus the command-layer error frame type.

/// Why a buffer could not be parsed into a command (or reply). A frame
/// that is merely incomplete is not an error: the parsers answer
/// `Ok(None)` for it.
///
/// ```
/// let e = kevy_resp::parse_command(b"*x\r\n").unwrap_err();
/// assert_eq!(e.to_string(), "malformed frame: bad multibulk count");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A malformed frame that can never become valid (e.g. bad length prefix).
    ///
    /// ```
    /// use kevy_resp::{ProtocolError, parse_command};
    ///
    /// let err = parse_command(b"*1\r\n$x\r\n").unwrap_err();
    /// assert!(matches!(err, ProtocolError::Malformed(_)));
    /// ```
    Malformed(&'static str),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed frame: {why}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// A command-layer error destined for the wire as a RESP error frame.
///
/// Carries the complete, already-prefixed message text (`ERR …` /
/// `WRONGTYPE …` / `INDEXBUILDING …`); the dispatch layer encodes it
/// verbatim into a `-<text>\r\n` reply. The dedicated type keeps parse
/// and dispatch helpers from using bare `&'static str` as an error
/// currency.
///
/// ```
/// let e = kevy_resp::CmdError::from("ERR syntax error");
/// assert_eq!(e.as_wire(), "ERR syntax error");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CmdError {
    /// The complete wire message for the error frame.
    ///
    /// ```
    /// use kevy_resp::{CmdError, encode_error};
    ///
    /// let CmdError::Wire(text) = CmdError::from("WRONGTYPE bad key") else { unreachable!() };
    /// let mut out = Vec::new();
    /// encode_error(&mut out, text);
    /// assert_eq!(out, b"-WRONGTYPE bad key\r\n");
    /// ```
    Wire(&'static str),
}

impl CmdError {
    /// The wire text encoded into the RESP error frame.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Wire(s) => s,
        }
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl std::error::Error for CmdError {}

impl From<&'static str> for CmdError {
    fn from(s: &'static str) -> Self {
        Self::Wire(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_frame_renders_its_reason_and_has_no_source() {
        let e = ProtocolError::Malformed("bad bulk length");
        assert_eq!(e.to_string(), "malformed frame: bad bulk length");
        assert!(std::error::Error::source(&e).is_none());
    }
}
