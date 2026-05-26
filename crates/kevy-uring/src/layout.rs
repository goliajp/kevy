//! `#[repr(C)]` wire-layout structs the kernel reads/writes through the
//! shared mmap regions. These are the io_uring ABI definitions translated
//! from `<linux/io_uring.h>`.

/// `struct io_sqring_offsets` — byte offsets of each SQ cursor inside the SQ
/// ring mapping, returned by `io_uring_setup`.
#[repr(C)]
#[derive(Default)]
pub struct IoSqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub resv2: u64,
}

/// `struct io_cqring_offsets` — byte offsets of each CQ cursor inside the CQ
/// ring mapping.
#[repr(C)]
#[derive(Default)]
pub struct IoCqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub resv2: u64,
}

/// `struct io_uring_params` — `io_uring_setup`'s in/out parameter.
#[repr(C)]
#[derive(Default)]
pub struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: IoSqringOffsets,
    pub cq_off: IoCqringOffsets,
}

/// `struct io_uring_sqe` — the 64-byte submission entry.
#[repr(C)]
pub struct IoUringSqe {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub rw_flags: u32,
    pub user_data: u64,
    pub buf_index: u16,
    pub personality: u16,
    pub splice_fd_in: i32,
    pub addr3: u64,
    pub __pad2: u64,
}

impl IoUringSqe {
    /// A zeroed SQE with the common fields set. Op-specific fields (e.g.
    /// `rw_flags` for accept flags) are tweaked by the caller afterward.
    pub fn new(opcode: u8, fd: i32, addr: u64, len: u32, user_data: u64) -> IoUringSqe {
        IoUringSqe {
            opcode,
            flags: 0,
            ioprio: 0,
            fd,
            off: 0,
            addr,
            len,
            rw_flags: 0,
            user_data,
            buf_index: 0,
            personality: 0,
            splice_fd_in: 0,
            addr3: 0,
            __pad2: 0,
        }
    }
}

/// `struct io_uring_buf_reg` — `io_uring_register(IORING_REGISTER_PBUF_RING,
/// …)`'s argument layout.
#[repr(C)]
pub struct IoUringBufReg {
    pub ring_addr: u64,
    pub ring_entries: u32,
    pub bgid: u16,
    pub pad: u16,
    pub resv: [u64; 3],
}
