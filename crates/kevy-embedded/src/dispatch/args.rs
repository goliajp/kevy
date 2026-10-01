//! The argv type the shared command layer (`kevy_verbs`) is run with.
//!
//! That layer is generic over the argv type, and every type it is run
//! with links every command once more. Natively the dispatcher's
//! `&[Vec<u8>]` and the replay's flat `Argv` each get their own instance,
//! each called without a conversion. On wasm the module is shipped over
//! the wire, so the two share one type here: a tag the reads branch on,
//! in place of about 50 KB of a second instance.

use kevy_resp::ArgvView;

/// An argv as the shared command layer reads it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Args<'a>(&'a [Vec<u8>]);

#[cfg(not(target_arch = "wasm32"))]
impl<'a> Args<'a> {
    pub(crate) fn new(argv: &'a [Vec<u8>]) -> Self {
        Args(argv)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl core::ops::Index<usize> for Args<'_> {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        &self.0[i]
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ArgvView for Args<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        self.0.get(i).map(Vec::as_slice)
    }
}

/// An argv as the shared command layer reads it: a client's, or a
/// logged frame being replayed.
#[cfg(target_arch = "wasm32")]
pub(crate) enum Args<'a> {
    Client(&'a [Vec<u8>]),
    #[cfg_attr(not(feature = "persist"), allow(dead_code, reason = "only a replay builds it"))]
    Frame(&'a kevy_resp::Argv),
}

#[cfg(target_arch = "wasm32")]
impl<'a> Args<'a> {
    pub(crate) fn new(argv: &'a [Vec<u8>]) -> Self {
        Args::Client(argv)
    }
}

#[cfg(target_arch = "wasm32")]
impl core::ops::Index<usize> for Args<'_> {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        match self {
            Args::Client(a) => &a[i],
            Args::Frame(a) => &a[i],
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl ArgvView for Args<'_> {
    fn len(&self) -> usize {
        match self {
            Args::Client(a) => a.len(),
            Args::Frame(a) => a.len(),
        }
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        match self {
            Args::Client(a) => a.get(i).map(Vec::as_slice),
            Args::Frame(a) => a.get(i),
        }
    }
}
