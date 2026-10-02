//! Without `std` there is no bio thread: displaced heavy values drop
//! inline on the caller.

use crate::Store;
use crate::value::Value;

impl Store {
    #[inline]
    pub(crate) fn maybe_offload_drop(&mut self, old: Value) {
        drop(old);
    }
}
