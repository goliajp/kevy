//! Asking the server without printing: what a mode needs from a session.

use crate::rcli::conn::LinkError;
use crate::rcli::session::Session;
use kevy_resp::{Reply, encode_command_borrowed};

impl Session {
    /// Send `argv` and read its reply, unprinted. Pushes that arrive first
    /// are not the reply and are skipped.
    pub(crate) fn request(&mut self, argv: &[&[u8]]) -> Result<Reply, LinkError> {
        let mut replies = self.pipeline(&[argv.to_vec()])?;
        replies.pop().ok_or(LinkError::Eof)
    }

    /// Send every command in one write, then read one reply for each, in
    /// order: a batch costs one round trip, not one per key.
    pub(crate) fn pipeline(&mut self, commands: &[Vec<&[u8]>]) -> Result<Vec<Reply>, LinkError> {
        let conn = self.conn.as_mut().ok_or(LinkError::Eof)?;
        let mut frame = Vec::new();
        for argv in commands {
            encode_command_borrowed(&mut frame, argv);
        }
        conn.write_raw(&frame)?;
        let mut replies = Vec::with_capacity(commands.len());
        while replies.len() < commands.len() {
            match conn.read_reply()? {
                (Reply::Push(_), _) => {}
                (reply, _) => replies.push(reply),
            }
        }
        Ok(replies)
    }
}
