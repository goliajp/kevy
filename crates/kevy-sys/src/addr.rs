//! Socket-level constants and `sockaddr` layouts (IPv4 + AF_UNIX), declared
//! per-OS to match the platform libc ABI. Split out of `lib.rs` to keep it
//! under the 500-LOC house cap; every value is verbatim from before the move.

use core::ffi::c_int;
use core::mem::size_of;
use std::io;

// ---- constants -------------------------------------------------------------

pub(crate) const AF_INET: c_int = 2;
pub(crate) const AF_UNIX: c_int = 1;
pub(crate) const SOCK_STREAM: c_int = 1;
pub(crate) const IPPROTO_TCP: c_int = 6;
pub(crate) const TCP_NODELAY: c_int = 1;
pub(crate) const F_GETFL: c_int = 3;
pub(crate) const F_SETFL: c_int = 4;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const SOL_SOCKET: c_int = 1;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const SO_REUSEADDR: c_int = 2;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const SO_REUSEPORT: c_int = 15;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const O_NONBLOCK: c_int = 0x800;
// MSG_PEEK is 2 on every Unix; MSG_DONTWAIT differs by platform.
pub(crate) const MSG_PEEK: c_int = 2;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const MSG_DONTWAIT: c_int = 0x40;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const SOL_SOCKET: c_int = 0xffff;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const SO_REUSEADDR: c_int = 0x0004;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const SO_REUSEPORT: c_int = 0x0200;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const O_NONBLOCK: c_int = 0x0004;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const MSG_DONTWAIT: c_int = 0x80;

// ---- sockaddr_in -----------------------------------------------------------

// struct_field_names: the `sin_` prefix mirrors the C ABI struct verbatim —
// dropping it would obscure the 1:1 layout correspondence.
#[allow(clippy::struct_field_names)]
#[cfg(any(target_os = "linux", target_os = "android"))]
#[repr(C)]
pub(crate) struct SockaddrIn {
    pub(crate) sin_family: u16,
    pub(crate) sin_port: u16,
    pub(crate) sin_addr: u32,
    pub(crate) sin_zero: [u8; 8],
}

// Field names mirror BSD's `<netinet/in.h>` struct sockaddr_in — the `sin_*`
// prefix is the ABI; renaming would just obscure the libc binding.
#[allow(clippy::struct_field_names)]
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
pub(crate) struct SockaddrIn {
    pub(crate) sin_len: u8,
    pub(crate) sin_family: u8,
    pub(crate) sin_port: u16,
    pub(crate) sin_addr: u32,
    pub(crate) sin_zero: [u8; 8],
}

impl SockaddrIn {
    pub(crate) fn new(ip: [u8; 4], port: u16) -> Self {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return SockaddrIn {
            sin_family: AF_INET as u16,
            sin_port: port.to_be(),
            sin_addr: u32::from_ne_bytes(ip),
            sin_zero: [0; 8],
        };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        return SockaddrIn {
            sin_len: size_of::<SockaddrIn>() as u8,
            sin_family: AF_INET as u8,
            sin_port: port.to_be(),
            sin_addr: u32::from_ne_bytes(ip),
            sin_zero: [0; 8],
        };
    }

    pub(crate) fn zeroed() -> Self {
        // SAFETY: `SockaddrIn` is `repr(C)` over integer and array fields only — no
        // references, no `NonNull`, no padding whose value is read — so the all-zero bit
        // pattern is a valid inhabitant. That is what the kernel expects to be handed
        // before it fills the struct in.
        unsafe { core::mem::zeroed() }
    }
}

// ---- sockaddr_un (AF_UNIX) -------------------------------------------------

/// The platform's `sun_path` capacity. Linux's `<sys/un.h>` gives 108;
/// every BSD, macOS included, gives 104. This was declared as 108 for
/// both, under a comment asserting that was correct for macOS — so the
/// struct was four bytes too long there, `sun_len` was set to a size the
/// kernel does not use, and [`SockaddrUn::new`] accepted paths four
/// bytes longer than the platform can hold.
///
/// It was benign: xnu bounds by `sun_len` into a larger buffer, so an
/// oversized struct still binds. What it was not is *true*, and this
/// crate's header claims these bindings match the platform ABI.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const SUN_PATH_CAP: usize = 108;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) const SUN_PATH_CAP: usize = 104;

/// Unix-domain `sockaddr_un`.
// struct_field_names: the `sun_` prefix mirrors the C ABI struct verbatim —
// dropping it would obscure the 1:1 layout correspondence.
#[allow(clippy::struct_field_names)]
#[repr(C)]
pub(crate) struct SockaddrUn {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) sun_family: u16,
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(crate) sun_len: u8,
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(crate) sun_family: u8,
    pub(crate) sun_path: [u8; SUN_PATH_CAP],
}

impl SockaddrUn {
    pub(crate) fn new(path: &[u8]) -> io::Result<(Self, u32)> {
        if path.is_empty() || path.len() >= SUN_PATH_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unix socket path must be 1..={} bytes", SUN_PATH_CAP - 1),
            ));
        }
        let mut sun_path = [0u8; SUN_PATH_CAP];
        sun_path[..path.len()].copy_from_slice(path);
        // The actual length passed to bind() is offset_of(sun_path) + strlen(path) + 1
        // (for the NUL); using full struct size also works on Linux + BSD.
        let sa = SockaddrUn {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            sun_family: AF_UNIX as u16,
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            sun_len: size_of::<SockaddrUn>() as u8,
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            sun_family: AF_UNIX as u8,
            sun_path,
        };
        Ok((sa, size_of::<SockaddrUn>() as u32))
    }
}

// ─────────────────────────────────────────────────────────────────────
// ABI assertions.
//
// These structs are handed to the kernel. A field added, a type widened
// or an alignment changed does not fail a test — it produces a syscall
// reading the wrong memory, on whichever platform nobody happened to
// develop on. There were none of these, and `sun_path` was already
// wrong: declared 108 bytes on a platform whose own header says 104,
// under a comment asserting otherwise.
//
// A wrong layout is a build failure now. Verified red-green: moving
// SUN_PATH_CAP by one byte fails the build with the message below.
// ─────────────────────────────────────────────────────────────────────

const _: () = assert!(size_of::<SockaddrIn>() == 16, "sockaddr_in is 16 bytes everywhere");
const _: () = assert!(align_of::<SockaddrIn>() == 4);

#[cfg(any(target_os = "linux", target_os = "android"))]
const _: () = assert!(size_of::<SockaddrUn>() == 110, "2-byte family + 108 path");
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const _: () = assert!(size_of::<SockaddrUn>() == 106, "1-byte len + 1-byte family + 104 path");

// The path must fit inside what the struct declares, or `new` writes
// past it. Stated rather than left to follow from the definition, so an
// edit to either one has to keep them agreeing.
const _: () = assert!(SUN_PATH_CAP < size_of::<SockaddrUn>());
