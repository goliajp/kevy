//! Numbers as text, read and written exactly as the C library does:
//! [`strtod`]'s reading of decimal, hexadecimal and named literals, and
//! the Grisu2 digits of a double laid out the way the fpconv library lays
//! them out.
//!
//! ```
//! let s = kevy_num::strtod(b"0x1p-2");
//! assert_eq!(s.value, 0.25);
//! let mut out = Vec::new();
//! kevy_num::write_grisu2(&mut out, 0.00012345);
//! assert_eq!(out, b"1.2345e-4");
//! ```

#![no_std]

extern crate alloc;

mod big;
mod dtoa;
mod ext;
mod round;
mod scan;
mod sqrt;

pub use dtoa::write_grisu2;
pub use ext::LongDouble;
pub use scan::{Scanned, parse_exact, strtod};
pub use sqrt::sqrt;
