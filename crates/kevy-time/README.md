# kevy-time

Pure calendar arithmetic on epoch seconds. Proleptic Gregorian, UTC only.

Every function is pure: `now` is always an argument, so the crate never
touches a clock — which is what makes date arithmetic testable rather than
flaky. Time values stay `i64` epoch seconds; this crate turns a human bound
into that `i64` and back, and never introduces a column type of its own.

Zone conversion is deliberately absent: it belongs to the application, and is
refused by name at the surface rather than silently guessed here.

```rust
use kevy_time::{Civil, civil_from_epoch, epoch_from_civil, add_months};

let c = civil_from_epoch(1_700_000_000);
assert_eq!(epoch_from_civil(c), 1_700_000_000);
```

The civil conversion is the standard integer-arithmetic algorithm
(era / year-of-era / day-of-year decomposition over 400-year cycles), exact
across the whole `i64` day range the epoch can reach — no floating point, no
lookup tables, no leap-year special cases.

Pure Rust, zero dependencies. Part of [kevy](https://github.com/goliajp/kevy).

Licensed under Apache-2.0 OR MIT.
