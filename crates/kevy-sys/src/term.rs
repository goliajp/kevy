//! Terminal control for an interactive client: raw input mode and the
//! window's width.
//!
//! `struct termios` differs between the two platforms kevy builds on — 60
//! bytes with `u32` flags and 32 control characters on Linux, 72 bytes with
//! `u64` flags and 20 on macOS — so each layout is written out and its size
//! and field offsets are asserted at compile time. The constants were read
//! from each platform's `<termios.h>` by a C program, not from memory.

use core::ffi::{c_int, c_ulong};
use std::io;
use std::os::fd::RawFd;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod abi {
    pub type Flag = u32;
    pub const NCCS: usize = 32;
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct Termios {
        pub iflag: Flag,
        pub oflag: Flag,
        pub cflag: Flag,
        pub lflag: Flag,
        pub line: u8,
        pub cc: [u8; NCCS],
        pub ispeed: u32,
        pub ospeed: u32,
    }
    const _: () = assert!(size_of::<Termios>() == 60);
    const _: () = assert!(core::mem::offset_of!(Termios, cc) == 17);
    const _: () = assert!(core::mem::offset_of!(Termios, ispeed) == 52);
    pub const ECHO: Flag = 0x8;
    pub const ICANON: Flag = 0x2;
    pub const ISIG: Flag = 0x1;
    pub const IEXTEN: Flag = 0x8000;
    pub const BRKINT: Flag = 0x2;
    pub const ICRNL: Flag = 0x100;
    pub const INPCK: Flag = 0x10;
    pub const ISTRIP: Flag = 0x20;
    pub const IXON: Flag = 0x400;
    pub const OPOST: Flag = 0x1;
    pub const CS8: Flag = 0x30;
    pub const VMIN: usize = 6;
    pub const VTIME: usize = 5;
    pub const TIOCGWINSZ: core::ffi::c_ulong = 0x5413;
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod abi {
    pub type Flag = u64;
    pub const NCCS: usize = 20;
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct Termios {
        pub iflag: Flag,
        pub oflag: Flag,
        pub cflag: Flag,
        pub lflag: Flag,
        pub cc: [u8; NCCS],
        pub ispeed: u64,
        pub ospeed: u64,
    }
    const _: () = assert!(size_of::<Termios>() == 72);
    const _: () = assert!(core::mem::offset_of!(Termios, cc) == 32);
    const _: () = assert!(core::mem::offset_of!(Termios, ispeed) == 56);
    pub const ECHO: Flag = 0x8;
    pub const ICANON: Flag = 0x100;
    pub const ISIG: Flag = 0x80;
    pub const IEXTEN: Flag = 0x400;
    pub const BRKINT: Flag = 0x2;
    pub const ICRNL: Flag = 0x100;
    pub const INPCK: Flag = 0x10;
    pub const ISTRIP: Flag = 0x20;
    pub const IXON: Flag = 0x200;
    pub const OPOST: Flag = 0x1;
    pub const CS8: Flag = 0x300;
    pub const VMIN: usize = 16;
    pub const VTIME: usize = 17;
    pub const TIOCGWINSZ: core::ffi::c_ulong = 0x4008_7468;
}

pub(crate) use abi::Termios;
use abi::*;

/// `TCSANOW`, the same on both platforms. Not `TCSAFLUSH`: flushing would
/// drop input already typed or pasted ahead of the editor.
const TCSANOW: c_int = 0;

/// `struct winsize`.
#[repr(C)]
#[derive(Debug, Default)]
pub(crate) struct Winsize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

const _: () = assert!(size_of::<Winsize>() == 8);

/// A terminal in raw input mode, restored to its previous mode on drop.
///
/// Raw means: bytes arrive one at a time as typed (no line buffering), are
/// not echoed, and Ctrl-C / Ctrl-Z / Ctrl-V arrive as bytes rather than as
/// signals — the settings a line editor needs to handle every key itself.
#[derive(Debug)]
pub struct RawMode {
    fd: RawFd,
    saved: Termios,
}

impl RawMode {
    /// Put the terminal on `fd` into raw mode.
    ///
    /// # Errors
    ///
    /// `ENOTTY` when `fd` is not a terminal; any other `tcgetattr` /
    /// `tcsetattr` error.
    ///
    /// # Examples
    ///
    /// ```
    /// // A pipe is not a terminal, so there is no mode to change.
    /// let (reader, _writer) = std::io::pipe()?;
    /// use std::os::fd::AsRawFd;
    /// assert!(kevy_sys::RawMode::enable(reader.as_raw_fd()).is_err());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn enable(fd: RawFd) -> io::Result<RawMode> {
        let saved = get(fd)?;
        let mut raw = saved;
        raw.iflag &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
        raw.oflag &= !OPOST;
        raw.cflag |= CS8;
        raw.lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
        raw.cc[VMIN] = 1;
        raw.cc[VTIME] = 0;
        set(fd, &raw)?;
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Restoring is best effort: the process is leaving the editor either
        // way, and a terminal that refuses its old mode has nobody to tell.
        let _ = set(self.fd, &self.saved);
    }
}

fn get(fd: RawFd) -> io::Result<Termios> {
    // SAFETY: an all-zero `Termios` is a valid value of a plain-data
    // `repr(C)` struct of integers.
    let mut t: Termios = unsafe { core::mem::zeroed() };
    // SAFETY: `t` is a live, exclusively borrowed `struct termios` whose
    // layout is asserted above; tcgetattr writes only within it.
    let rc = unsafe { crate::ffi::tcgetattr(fd, &mut t) };
    if rc == 0 { Ok(t) } else { Err(io::Error::last_os_error()) }
}

fn set(fd: RawFd, t: &Termios) -> io::Result<()> {
    // SAFETY: `t` points at a valid `struct termios` (layout asserted); the
    // call only reads it.
    let rc = unsafe { crate::ffi::tcsetattr(fd, TCSANOW, t) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The width in columns of the terminal on `fd`, when it reports one.
///
/// `None` for a descriptor that is not a terminal or a terminal that reports
/// zero columns (a pty nobody sized); a caller then assumes 80, as terminal
/// programs conventionally do.
///
/// # Examples
///
/// ```
/// let (reader, _writer) = std::io::pipe()?;
/// use std::os::fd::AsRawFd;
/// assert_eq!(kevy_sys::terminal_columns(reader.as_raw_fd()), None);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn terminal_columns(fd: RawFd) -> Option<u16> {
    let mut ws = Winsize::default();
    // SAFETY: TIOCGWINSZ writes one `struct winsize` (layout asserted) through
    // the pointer, which is a live exclusive borrow.
    let rc = unsafe { crate::ffi::ioctl(fd, TIOCGWINSZ as c_ulong, &mut ws as *mut Winsize) };
    (rc == 0 && ws.cols > 0).then_some(ws.cols)
}
