//! Each input here was given to glibc's `strtod`; the table holds the
//! double it returned (as bits), how many bytes it read, and whether it set
//! ERANGE.

fn unescape(s: &str) -> Vec<u8> {
    s.replace("\\t", "\t").replace("\\n", "\n").replace("\\\\", "\\").into_bytes()
}

#[test]
fn reads_what_glibc_reads() {
    let table = include_str!("fixtures/glibc-strtod.tsv");
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in table.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let input = unescape(cols[0]);
        let bits = u64::from_str_radix(cols[1], 16).expect("hex bits");
        let len: usize = cols[2].parse().expect("a length");
        let erange = cols[3] == "1";
        let want = f64::from_bits(bits);
        // ERANGE also marks a subnormal result; only an overflow or a
        // nonzero literal lost to zero is out of range here
        let out_of_range = erange && (want.is_infinite() || want == 0.0);
        let got = kevy_num::strtod(&input);
        let same_value = got.value.to_bits() == bits || (got.value.is_nan() && want.is_nan());
        if !same_value || got.len != len || got.out_of_range != out_of_range {
            wrong.push(format!(
                "{:?}: glibc {:?} len {len} range {out_of_range}, kevy {:?} len {} range {}",
                cols[0], want, got.value, got.len, got.out_of_range
            ));
        }
        checked += 1;
    }
    assert!(checked > 3000, "the table did not load");
    assert!(
        wrong.is_empty(),
        "{} of {checked} differ:\n{}",
        wrong.len(),
        wrong[..wrong.len().min(25)].join("\n")
    );
}
