//! A shared, writable mapping of a file: stores into it land in the page
//! cache with no syscall, and they outlive the process that made them —
//! the kernel owns the pages, so a crash or a kill loses none of them.
//! Power loss is another matter: only `sync` puts them on the medium.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
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
///
/// ```
/// use std::io::Read;
/// let path = std::env::temp_dir().join(format!("filemap-doc-{}", std::process::id()));
/// let mut f = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&path)?;
/// f.set_len(4096)?;
/// let mut map = kevy_sys::FileMap::map(&f, 4096)?;
/// map.bytes_mut(0, 4).copy_from_slice(b"kevy");
/// map.sync()?;
/// let mut head = [0u8; 4];
/// f.read_exact(&mut head)?;
/// assert_eq!(&head, b"kevy", "a store into the mapping is in the file");
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct FileMap {
    region: Arc<Region>,
}

/// A handle that can only put a mapping's pages on the medium. It keeps
/// the mapping alive, so a sync can run on another thread while the
/// [`FileMap`] goes on being written — `msync` is the kernel reading the
/// pages, not an access by this process.
///
/// ```
/// let path = std::env::temp_dir().join(format!("mapsync-doc-{}", std::process::id()));
/// let f = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&path)?;
/// f.set_len(65536)?;
/// let mut map = kevy_sys::FileMap::map(&f, 65536)?;
/// let handle = map.sync_handle();
/// let syncer = std::thread::spawn(move || handle.sync());
/// map.bytes_mut(100, 3).copy_from_slice(b"new");
/// syncer.join().unwrap()?;
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct MapSync {
    region: Arc<Region>,
}

/// The mapped range itself; unmapped when the last holder drops it.
#[derive(Debug)]
struct Region {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the region is plain memory owned by the mapping; nothing about it
// is tied to the thread that created it.
unsafe impl Send for Region {}
// SAFETY: a shared `Region` is only read for its address and length, and
// passed to `msync`; the bytes are reached through `FileMap`'s borrows.
unsafe impl Sync for Region {}

impl Region {
    fn sync(&self) -> io::Result<()> {
        // SAFETY: the range is exactly the live mapping this region owns.
        if unsafe { msync(self.ptr.cast(), self.len, MS_SYNC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the range is the mapping `map_at` returned, and this is its
        // last holder, so no pointer into it is used afterwards.
        unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

impl MapSync {
    /// Write every dirty page of the mapping to the file and wait
    /// (`msync(MS_SYNC)`).
    pub fn sync(&self) -> io::Result<()> {
        self.region.sync()
    }
}

impl FileMap {
    /// Map the first `len` bytes of `file`, which must be open for reading
    /// and writing and at least `len` bytes long.
    pub fn map(file: &File, len: usize) -> io::Result<FileMap> {
        FileMap::map_at(file, 0, len)
    }

    /// Map `len` bytes of `file` starting at `offset`, which must be a
    /// multiple of the page size (64 KiB is one on every platform kevy
    /// runs on). The file must reach `offset + len`.
    pub fn map_at(file: &File, offset: u64, len: usize) -> io::Result<FileMap> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty mapping"));
        }
        let offset = i64::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset past i64"))?;
        // SAFETY: a null hint, a length, flags and a live fd by value; the
        // kernel picks the address and reports failure as MAP_FAILED.
        let p = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                offset,
            )
        };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileMap { region: Arc::new(Region { ptr: p.cast(), len }) })
    }

    /// The first byte of the mapping; valid for `len()` bytes while `self`
    /// lives.
    pub fn as_ptr(&self) -> *mut u8 {
        self.region.ptr
    }

    /// The mapping's length in bytes.
    pub fn len(&self) -> usize {
        self.region.len
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
        assert!(
            off.checked_add(len).is_some_and(|e| e <= self.region.len),
            "read past the mapping"
        );
        // SAFETY: the range was just checked against the live mapping, and
        // the borrow of `self` keeps the mapping alive and unwritten through
        // this value for as long as the slice lives.
        unsafe { std::slice::from_raw_parts(self.region.ptr.add(off), len) }
    }

    /// `len` writable bytes at `off`. Panics when the range leaves the
    /// mapping.
    pub fn bytes_mut(&mut self, off: usize, len: usize) -> &mut [u8] {
        assert!(
            off.checked_add(len).is_some_and(|e| e <= self.region.len),
            "write past the mapping"
        );
        // SAFETY: the range was just checked against the live mapping, and
        // the exclusive borrow of `self` rules out any other view of it.
        unsafe { std::slice::from_raw_parts_mut(self.region.ptr.add(off), len) }
    }

    /// The 8 bytes at `off` as an atomic. Panics when `off` is not a
    /// multiple of 8 or the cell leaves the mapping. Those bytes should
    /// then be read and written only through the atomic.
    pub fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        assert!(off.is_multiple_of(8) && off + 8 <= self.region.len, "a misplaced atomic cell");
        // SAFETY: the mapping is page-aligned, so an offset that is a multiple
        // of 8 is 8-byte aligned; the cell lies inside the live mapping, which
        // the borrow of `self` keeps alive.
        unsafe { &*self.region.ptr.add(off).cast::<AtomicU64>() }
    }

    /// Write every dirty page of the mapping to the file and wait
    /// (`msync(MS_SYNC)`).
    pub fn sync(&self) -> io::Result<()> {
        self.region.sync()
    }

    /// A handle that syncs this mapping from anywhere, keeping it mapped.
    pub fn sync_handle(&self) -> MapSync {
        MapSync { region: Arc::clone(&self.region) }
    }
}

/// Reserve `len` bytes of disk past the file's allocated end, so pages
/// later written through a mapping land on blocks that already exist
/// (`F_PREALLOCATE`). Elsewhere there is nothing to ask, and it is a no-op.
///
/// ```
/// let path = std::env::temp_dir().join(format!("prealloc-doc-{}", std::process::id()));
/// let f = std::fs::File::create(&path)?;
/// kevy_sys::preallocate(&f, 1 << 20)?;
/// assert_eq!(f.metadata()?.len(), 0, "reserving space does not grow the file");
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    #[repr(C)]
    struct FStore {
        fst_flags: u32,
        fst_posmode: c_int,
        fst_offset: i64,
        fst_length: i64,
        fst_bytesalloc: i64,
    }
    const F_PREALLOCATE: c_int = 42;
    const F_ALLOCATEALL: u32 = 4;
    const F_PEOFPOSMODE: c_int = 3;
    let mut store = FStore {
        fst_flags: F_ALLOCATEALL,
        fst_posmode: F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: i64::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length past i64"))?,
        fst_bytesalloc: 0,
    };
    // SAFETY: F_PREALLOCATE takes a pointer to an fstore_t, which `FStore`
    // lays out field for field; it lives across the call.
    let rc =
        unsafe { crate::ffi::fcntl(file.as_raw_fd(), F_PREALLOCATE, &mut store as *mut FStore) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Nothing to reserve ahead on this platform.
///
/// ```
/// let path = std::env::temp_dir().join(format!("prealloc-doc-{}", std::process::id()));
/// let f = std::fs::File::create(&path)?;
/// kevy_sys::preallocate(&f, 1 << 20)?;
/// assert_eq!(f.metadata()?.len(), 0, "reserving space does not grow the file");
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub fn preallocate(_file: &File, _len: u64) -> io::Result<()> {
    Ok(())
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
    fn a_mapping_at_an_offset_sees_that_part_of_the_file() {
        let path = tmp("offset");
        let f =
            File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(128 * 1024).unwrap();
        let mut m = FileMap::map_at(&f, 64 * 1024, 64 * 1024).unwrap();
        m.bytes_mut(10, 4).copy_from_slice(b"kevy");
        m.sync().unwrap();
        drop(m);
        let mut got = [0u8; 4];
        use std::os::unix::fs::FileExt;
        f.read_exact_at(&mut got, 64 * 1024 + 10).unwrap();
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
