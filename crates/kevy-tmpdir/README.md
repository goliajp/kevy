# kevy-tmpdir

A temporary directory that is actually unique, and cleans up after itself.

This crate exists because nine places in one workspace had each invented their
own, using five different ways of trying to make the name unique — and **none
of them was unique**:

- `process::id()` is the *same* for every test in a binary: cargo runs tests as
  threads, not processes. Two tests in one file get the same directory and
  stamp on each other's files.
- A clock is unique only until two threads read it inside the same tick, which
  under parallel test execution is exactly what happens.

Both produce a flake: green on a quiet machine, red under load, and it looks
like the code under test is broken rather than the fixture.

```rust
use kevy_tmpdir::TmpDir;

let dir = TmpDir::new("my-test");
std::fs::write(dir.path().join("f"), b"x").unwrap();
// removed when `dir` drops, including on panic
```

Uniqueness comes from **a process id and a monotonic counter together**, which
is the one combination that closes both holes: the counter separates threads
within a binary (where the pid is identical), and the pid separates concurrent
binaries (where counters both start at zero).

Removal is RAII — it happens on unwind too, so a failing test does not leave
residue behind for the next run to trip over.

Pure Rust, zero dependencies. Part of [kevy](https://github.com/goliajp/kevy).

Licensed under Apache-2.0 OR MIT.
