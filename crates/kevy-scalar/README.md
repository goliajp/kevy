# kevy-scalar

PostgreSQL-canonical scalar function evaluation over typed values.

The function library behind kevy's SQL face: `SELECT lower('X')`-shaped
constant folding and the query-card projection epilogue both call `eval`.

This crate knows nothing about kevy. It maps a function name and `Scalar`
arguments to a `Scalar` result with PostgreSQL 18 semantics, and that is the
whole contract — evaluation stays in the SQL face by design and never runs
inside a serving engine process.

```rust
use kevy_scalar::{eval, Scalar};

let out = eval("lower", &[Scalar::Text("HeLLo".into())]).unwrap();
assert_eq!(out, Scalar::Text("hello".into()));
```

## Ground truth

Semantics come from a `pg_regress`-derived probe corpus, not from reading the
documentation. The tests are transcribed from those files, and where PostgreSQL
is surprising the probe line is cited in place:

- `floor` rounds toward −infinity, not toward zero
- `trim` strips a character **set**, not a substring
- NULL propagates through almost everything

Pure Rust, zero dependencies. Part of [kevy](https://github.com/goliajp/kevy).

Licensed under Apache-2.0 OR MIT.
