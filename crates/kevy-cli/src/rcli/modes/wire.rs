//! Asking the server without printing: what a mode needs from a session.

use crate::rcli::conn::LinkError;
use crate::rcli::session::Session;
use kevy_resp::Reply;

impl Session {
    /// Send `argv` and read its reply, unprinted. Pushes that arrive first
    /// are not the reply and are skipped.
    pub(crate) fn request(&mut self, argv: &[&[u8]]) -> Result<Reply, LinkError> {
        self.conn.as_mut().ok_or(LinkError::Eof)?.request(argv)
    }

    /// Send every command in one write, then read one reply for each, in
    /// order: a batch costs one round trip, not one per key.
    pub(crate) fn pipeline(&mut self, commands: &[Vec<&[u8]>]) -> Result<Vec<Reply>, LinkError> {
        self.conn.as_mut().ok_or(LinkError::Eof)?.pipeline(commands)
    }
}
