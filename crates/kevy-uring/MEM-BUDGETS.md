# kevy-uring — memory budgets

`kevy-uring` allocates a small fixed amount per ring at construction time
and **nothing** per submitted operation. This is the whole reason for
running through io_uring instead of a per-read syscall path.

## Per-`IoUring` allocation (constructor)

| Region | Size | Source | Owner |
|---|---|---|---|
| SQ ring (mmap)  | `params.sq_entries * sizeof(u32)` + per-ring headers, rounded up to a page | kernel via `mmap(MAP_SHARED)` | kernel-shared |
| CQ ring (mmap)  | `params.cq_entries * sizeof(IoUringCqe)` + per-ring headers, rounded up to a page | kernel via `mmap(MAP_SHARED)` | kernel-shared |
| SQE region (mmap) | `params.sq_entries * sizeof(IoUringSqe)` (`= entries * 64`), rounded up to a page | kernel via `mmap(MAP_SHARED)` | kernel-shared |

Total at `IoUring::new(entries)` is bounded by ~`entries * (4 + 16 + 64)
≈ 84 * entries` bytes, page-rounded. For the typical 64-entry ring,
three pages (≈ 12 KiB on x86_64 4-KiB pages).

Rust heap: zero — the `IoUring` struct holds three raw pointers plus a
few `u32` cursors. Drop unmaps the three regions and closes the ring
fd.

## Per-`ProvidedBufRing` allocation

| Region | Size | Source |
|---|---|---|
| Provided-buffer ring (mmap) | one page | kernel via `mmap(MAP_SHARED \| MAP_POPULATE)` |
| Slab (Rust `Vec<u8>`)       | `entries * buf_size` | Rust heap |

Drop unregisters the ring with the kernel (`IORING_UNREGISTER_PBUF_RING`)
and `munmap`s. The `Vec` is freed normally.

## Per-operation allocation

**Zero.** `prep_nop` / `prep_read` / `prep_write` / `prep_accept` /
`prep_recv_multishot` write directly into the SQE slot the engine just
acquired; submission writes the SQ-tail atomic; completion read clears
the CQE atomically. Nothing is heap-allocated on the submit path or the
reap path.

## Reproducer

```bash
# Linux only; macOS is a no-op crate.
cargo test -p kevy-uring --release
# Inspect with strace -e mmap,munmap,io_uring_setup,io_uring_enter
```
