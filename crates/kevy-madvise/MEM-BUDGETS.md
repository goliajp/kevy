# kevy-madvise — memory budgets

`kevy-madvise` has no Rust-owned heap allocations. The only operation
the crate performs is a single best-effort `madvise(2)` syscall —
strictly a kernel hint, with no per-call Rust-side state.

## Per-call allocation

- **Heap**: 0 bytes allocated, 0 bytes retained.
- **Stack**: a handful of usizes during the alignment math + `c_int` for
  the syscall arg.

## Caller-side effect

The whole point of the call is to ask the Linux kernel to promote 2 MB
transparent huge pages over the caller's region. That promotion happens
in kernel memory (not Rust heap); the caller's allocation is unchanged
in size, just backed by larger PTEs over time as the kernel's
`khugepaged` thread runs.

## Reproducer

```bash
# No bench yet; size is trivially asserted by inspection. The wrapper
# is one syscall; the rest is integer arithmetic on the inputs.
cargo expand -p kevy-madvise --lib
```
