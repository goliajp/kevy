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
        unsafe { core::mem::zeroed() }
    }
}

// ---- sockaddr_un (AF_UNIX) -------------------------------------------------

/// Unix-domain `sockaddr_un`. sun_path is 108 bytes on Linux + macOS BSD.
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
    pub(crate) sun_path: [u8; 108],
}

impl SockaddrUn {
    pub(crate) fn new(path: &[u8]) -> io::Result<(Self, u32)> {
        if path.is_empty() || path.len() >= 108 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unix socket path must be 1..=107 bytes",
            ));
        }
        let mut sun_path = [0u8; 108];
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
