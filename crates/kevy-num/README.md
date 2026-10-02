# kevy-num

Numbers as text, read and written exactly as the C library does.

- `strtod` reads a number the way C's `strtod` reads it: leading white
  space, a sign, then a decimal, a hexadecimal (`0x1.8p3`), `inf` /
  `infinity` or `nan` in any case, taking the longest prefix that is one,
  correctly rounded, and saying whether the value fell out of the double's
  range. Checked against glibc's own `strtod` on several thousand inputs.
- `parse_exact` takes a whole input that is one such literal and nothing
  else.
- `write_grisu2` prints a double's Grisu2 digits (Loitsch, "Printing
  Floating-Point Numbers Quickly and Accurately with Integers") as an
  integer, a plain decimal or scientific notation, the way the fpconv
  library lays them out. The digits always read back to the same double,
  though not always the shortest such digits.
- `LongDouble` is the x87 80-bit extended format, `long double` on x86-64:
  read from the same literals, added, and printed as `printf("%.Nf")`
  prints it, every step rounded exactly as glibc rounds it. Checked
  against glibc's `strtold`, `+` and `printf` on several thousand pairs.

```rust
let s = kevy_num::strtod(b"  0x1.8p1 rest");
assert_eq!((s.value, s.len), (3.0, 9));

let mut out = Vec::new();
kevy_num::write_grisu2(&mut out, 0.00012345);
assert_eq!(out, b"1.2345e-4");

let sum = kevy_num::LongDouble::parse_exact(b"1.1").unwrap()
    + kevy_num::LongDouble::parse_exact(b"2.2").unwrap();
out.clear();
sum.write_fixed(&mut out, 17);
assert_eq!(out, b"3.30000000000000000");
```

Pure Rust, `no_std` with `alloc`, zero dependencies. Part of
[kevy](https://github.com/goliajp/kevy).

Licensed under Apache-2.0 OR MIT.
