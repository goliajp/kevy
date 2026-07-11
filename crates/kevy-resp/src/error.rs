//! Protocol-level parsing error shared by request + reply parsers,
//! plus the command-layer error frame type.

/// Why a buffer could not (yet) be parsed into a command (or reply).
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// A malformed frame that can never become valid (e.g. bad length prefix).
    Malformed(&'static str),
}

/// A command-layer error destined for the wire as a RESP error frame.
///
/// Carries the complete, already-prefixed message text (`ERR …` /
/// `WRONGTYPE …` / `INDEXBUILDING …`); the dispatch layer encodes it
/// verbatim into a `-<text>\r\n` reply. The dedicated type keeps parse
/// and dispatch helpers from using bare `&'static str` as an error
/// currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdError {
    /// The complete wire message for the error frame.
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
