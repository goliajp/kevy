//! Protocol-level parsing error shared by request + reply parsers.

/// Why a buffer could not (yet) be parsed into a command (or reply).
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// A malformed frame that can never become valid (e.g. bad length prefix).
    Malformed(&'static str),
}
