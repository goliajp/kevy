//! Provided-buffer ring — the destination pool for multishot `recv`. The
//! kernel draws a buffer per arrival and reports its id; the app recycles
//! the buffer once the bytes are copied out.

use core::ffi::{c_int, c_long, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU16, Ordering};
use std::io;

use crate::ffi::{
    self, IO_URING_BUF_SIZE, IO_URING_BUF_TAIL_OFF, IORING_REGISTER_PBUF_RING,
    IORING_UNREGISTER_PBUF_RING, MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE,
    SYS_IO_URING_REGISTER,
};
use crate::layout::IoUringBufReg;

/// A registered provided-buffer ring (the destination pool for multishot
/// [`recv`](crate::IoUring::prep_recv_multishot)). Owns the buf-ring mapping
/// and the backing slab; the kernel fills a buffer per arrival, the app
/// recycles it.
pub struct ProvidedBufRing {
    pub(crate) ring_fd: c_int,
    pub(crate) ring: *mut u8,
    pub(crate) ring_len: usize,
    /// Contiguous backing store; buffer `bid` is `slab[bid*buf_size ..][..n]`.
    /// Never resized, so the addresses published into the ring stay valid.
    pub(crate) slab: Vec<u8>,
    pub(crate) mask: u16,
    pub(crate) buf_size: u32,
    pub(crate) bgid: u16,
    /// Local producer cursor (published to the kernel by [`Self::commit`]).
    pub(crate) tail: u16,
}

// SAFETY: like `IoUring`, a single owner per shard thread; not `Sync`.
unsafe impl Send for ProvidedBufRing {}

impl ProvidedBufRing {
    /// Allocate a provided-buffer ring and register it with the kernel under
    /// `bgid`. Called from [`IoUring::register_buf_ring`](crate::IoUring::register_buf_ring).
    pub(crate) fn new(
        ring_fd: c_int,
        entries: u16,
        buf_size: u32,
        bgid: u16,
    ) -> io::Result<Self> {
        assert!(entries.is_power_of_two(), "buf ring entries must be power of two");
        let (ring, ring_len) = Self::mmap_buf_ring(entries)?;
        if let Err(e) = Self::register_with_kernel(ring_fd, ring, entries, bgid) {
            // Unmap on failure so we don't leak the page.
            // SAFETY: `ring`/`ring_len` are the pair `mmap` just returned to us.
            unsafe { ffi::munmap(ring as *mut c_void, ring_len) };
            return Err(e);
        }
        let mut pbr = ProvidedBufRing {
            ring_fd,
            ring,
            ring_len,
            slab: vec![0u8; entries as usize * buf_size as usize],
            mask: entries - 1,
            buf_size,
            bgid,
            tail: 0,
        };
        // Publish all buffers so the first recvs have somewhere to land.
        for bid in 0..entries {
            pbr.stage(bid);
        }
        pbr.commit();
        Ok(pbr)
    }

    /// Allocate the `entries × io_uring_buf` page-aligned region.
    fn mmap_buf_ring(entries: u16) -> io::Result<(*mut u8, usize)> {
        let ring_len = entries as usize * IO_URING_BUF_SIZE;
        // SAFETY: anonymous mmap with valid PROT/flag bitset and length > 0.
        let ring = unsafe {
            ffi::mmap(
                ptr::null_mut(),
                ring_len,
                PROT_READ | PROT_WRITE,
                MAP_ANONYMOUS | MAP_PRIVATE,
                -1,
                0,
            )
        };
        if ring as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok((ring as *mut u8, ring_len))
    }

    /// Tell the kernel about a newly allocated buf ring under `bgid`.
    fn register_with_kernel(ring_fd: c_int, ring: *mut u8, entries: u16, bgid: u16) -> io::Result<()> {
        let reg = IoUringBufReg {
            ring_addr: ring as u64,
            ring_entries: entries as u32,
            bgid,
            pad: 0,
            resv: [0; 3],
        };
        // SAFETY: `reg` lives through the syscall; ring_fd is a valid io_uring fd.
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_REGISTER,
                ring_fd as c_long,
                IORING_REGISTER_PBUF_RING as c_long,
                &reg as *const IoUringBufReg as c_long,
                1 as c_long,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The buffer group id this ring serves (pass to `prep_recv_multishot`).
    pub fn group(&self) -> u16 {
        self.bgid
    }

    /// The `n` valid bytes the kernel placed in buffer `bid` (n = completion `res`).
    pub fn bytes(&self, bid: u16, n: usize) -> &[u8] {
        let start = bid as usize * self.buf_size as usize;
        &self.slab[start..start + n.min(self.buf_size as usize)]
    }

    /// Place buffer `bid` at the current tail slot (without publishing). Writes
    /// only addr/len/bid (offsets 0..14), never the tail at offset 14.
    pub(crate) fn stage(&mut self, bid: u16) {
        let idx = (self.tail & self.mask) as usize;
        // SAFETY: `idx` is masked into [0, entries), so `base` is in-bounds.
        let base = unsafe { self.ring.add(idx * IO_URING_BUF_SIZE) };
        // SAFETY: `bid` * buf_size is < slab.len() by construction.
        let addr = unsafe { self.slab.as_ptr().add(bid as usize * self.buf_size as usize) } as u64;
        // SAFETY: `base` is an in-bounds 16-byte slot; fields are unaligned-safe.
        unsafe {
            ptr::write_unaligned(base as *mut u64, addr);
            ptr::write_unaligned(base.add(8) as *mut u32, self.buf_size);
            ptr::write_unaligned(base.add(12) as *mut u16, bid);
        }
        self.tail = self.tail.wrapping_add(1);
    }

    /// Publish the staged buffers to the kernel (store-release on the ring tail).
    pub(crate) fn commit(&self) {
        // SAFETY: `ring + IO_URING_BUF_TAIL_OFF` is a 2-byte slot inside the
        // mapping (offset 14 of slot 0 — see IO_URING_BUF_TAIL_OFF doc).
        let tail = unsafe { &*(self.ring.add(IO_URING_BUF_TAIL_OFF) as *const AtomicU16) };
        tail.store(self.tail, Ordering::Release);
    }

    /// Return buffer `bid` to the ring so the kernel can reuse it. Call once the
    /// bytes from its completion have been copied out.
    pub fn recycle(&mut self, bid: u16) {
        self.stage(bid);
        self.commit();
    }
}

impl Drop for ProvidedBufRing {
    fn drop(&mut self) {
        // Best-effort unregister (EBADF if the ring fd is already closed — fine),
        // then unmap. The slab Vec frees itself.
        let reg = IoUringBufReg {
            ring_addr: 0,
            ring_entries: 0,
            bgid: self.bgid,
            pad: 0,
            resv: [0; 3],
        };
        // SAFETY: kernel-side cleanup; the mapping is ours to free.
        unsafe {
            ffi::syscall(
                SYS_IO_URING_REGISTER,
                self.ring_fd as c_long,
                IORING_UNREGISTER_PBUF_RING as c_long,
                &reg as *const IoUringBufReg as c_long,
                1 as c_long,
            );
            ffi::munmap(self.ring as *mut c_void, self.ring_len);
        }
    }
}
