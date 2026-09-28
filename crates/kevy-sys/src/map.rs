//! A shared, writable mapping of a file: stores into it land in the page
//! cache with no syscall, and they outlive the process that made them —
//! the kernel owns the pages, so a crash or a kill loses none of them.
//! Power loss is another matter: only `sync` puts them on the medium.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicU64;

use core::ffi::{c_int, c_void};

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
#[cfg(any(target_os = "linux", target_os = "android"))]
const MS_SYNC: c_int = 4;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const MS_SYNC: c_int = 0x10;

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn msync(addr: *mut c_void, len: usize, flags: c_int) -> c_int;
}

/// The first `len` bytes of a file, mapped shared and read-write.
///
/// The file must already be at least `len` bytes long: touching a mapped
/// page past the end of the file is a `SIGBUS`, not an error.
#[derive(Debug)]
pub struct FileMap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; moving it to
// another thread moves that ownership. Shared access goes through raw
// pointers the caller synchronises.
unsafe impl Send for FileMap {}
// SAFETY: `&FileMap` only hands out the base pointer and the length; any
// access through them is the caller's to synchronise.
unsafe impl Sync for FileMap {}

impl FileMap {
    /// Map the first `len` bytes of `file`, which must be open for reading
    /// and writing and at least `len` bytes long.
    pub fn map(file: &File, len: usize) -> io::Result<FileMap> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty mapping"));
        }
        // SAFETY: a null hint, a length, flags and a live fd by value; the
        // kernel picks the address and reports failure as MAP_FAILED.
        let p = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileMap { ptr: p.cast(), len })
    }

    /// The first byte of the mapping; valid for `len()` bytes while `self`
    /// lives.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The mapping's length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: a mapping is never empty.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// `len` bytes at `off`. Panics when the range leaves the mapping.
    ///
    /// Another process mapping the same file can change these bytes at any
    /// time; the file is this process's to write, as with any file it owns.
    pub fn bytes(&self, off: usize, len: usize) -> &[u8] {
        assert!(off.checked_add(len).is_some_and(|e| e <= self.len), "read past the mapping");
        // SAFETY: the range was just checked against the live mapping, and
        // the borrow of `self` keeps the mapping alive and unwritten through
        // this value for as long as the slice lives.
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }
    }

    /// `len` writable bytes at `off`. Panics when the range leaves the
    /// mapping.
    pub fn bytes_mut(&mut self, off: usize, len: usize) -> &mut [u8] {
        assert!(off.checked_add(len).is_some_and(|e| e <= self.len), "write past the mapping");
        // SAFETY: the range was just checked against the live mapping, and
        // the exclusive borrow of `self` rules out any other view of it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(off), len) }
    }

    /// The 8 bytes at `off` as an atomic. Panics when `off` is not a
    /// multiple of 8 or the cell leaves the mapping. Those bytes should
    /// then be read and written only through the atomic.
    pub fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        assert!(off.is_multiple_of(8) && off + 8 <= self.len, "a misplaced atomic cell");
        // SAFETY: the mapping is page-aligned, so an offset that is a multiple
        // of 8 is 8-byte aligned; the cell lies inside the live mapping, which
        // the borrow of `self` keeps alive.
        unsafe { &*self.ptr.add(off).cast::<AtomicU64>() }
    }

    /// Write every dirty page of the mapping to the file and wait
    /// (`msync(MS_SYNC)`).
    pub fn sync(&self) -> io::Result<()> {
        // SAFETY: the range is exactly the live mapping this value owns.
        if unsafe { msync(self.ptr.cast(), self.len, MS_SYNC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for FileMap {
    fn drop(&mut self) {
        // SAFETY: the range is the mapping `map` returned and nothing else
        // unmaps it; after this no pointer from `as_ptr` may be used, which
        // the borrow on `self` already guarantees for safe callers.
        unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kevy-sys-map-{name}-{}", std::process::id()))
    }

    #[test]
    fn a_store_through_the_mapping_is_in_the_file() {
        let path = tmp("store");
        let mut f =
            File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(8192).unwrap();
        let m = FileMap::map(&f, 8192).unwrap();
        // SAFETY: offset 5000 lies inside the 8192-byte mapping.
        unsafe { m.as_ptr().add(5000).copy_from_nonoverlapping(b"kevy".as_ptr(), 4) };
        m.sync().unwrap();
        drop(m);
        let mut got = [0u8; 4];
        f.seek(SeekFrom::Start(5000)).unwrap();
        f.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"kevy");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_empty_mapping_is_refused() {
        let path = tmp("empty");
        let f =
            File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        assert_eq!(FileMap::map(&f, 0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        std::fs::remove_file(&path).unwrap();
    }
}
