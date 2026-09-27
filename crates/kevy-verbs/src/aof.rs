//! What a write is recorded as, when the argv as run is not the right
//! record.

/// The record of an `SPOP` that removed `popped`: `SREM key member…`.
/// Replaying `SPOP` itself would draw different members.
///
/// ```
/// let f = kevy_verbs::aof::spop_effect(b"s", vec![b"a".to_vec()]);
/// assert_eq!(f, vec![b"SREM".to_vec(), b"s".to_vec(), b"a".to_vec()]);
/// ```
pub fn spop_effect(key: &[u8], popped: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut frame = Vec::with_capacity(2 + popped.len());
    frame.push(b"SREM".to_vec());
    frame.push(key.to_vec());
    frame.extend(popped);
    frame
}
