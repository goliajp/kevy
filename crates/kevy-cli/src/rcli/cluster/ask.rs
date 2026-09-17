//! Questions the cluster manager asks on stdin.

use crate::rcli::send::write_out;

/// Print `prompt` and read one line; `None` at end of input.
pub(crate) fn line(prompt: &[u8]) -> Option<Vec<u8>> {
    write_out(prompt);
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf.trim_end_matches(['\n', '\r']).as_bytes().to_vec()),
    }
}

/// `prompt (type 'yes' to accept): `; `true` only for `yes`.
pub(crate) fn confirm(prompt: &[u8]) -> bool {
    line(&[prompt, b" (type 'yes' to accept): "].concat()).is_some_and(|l| l == b"yes")
}
