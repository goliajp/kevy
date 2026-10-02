//! Each pair here was read by glibc's `strtold` on x86-64 (refused when it
//! was not the whole input, had white space in front, was NaN, or fell out
//! of range), and when both were read their sum printed with
//! `printf("%.17Lf")`.

use kevy_num::LongDouble;

#[test]
fn reads_adds_and_prints_as_glibc_does() {
    let table = include_str!("fixtures/glibc-long-double.tsv");
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in table.lines() {
        let c: Vec<&str> = line.split('\t').collect();
        let (a, b) =
            (LongDouble::parse_exact(c[0].as_bytes()), LongDouble::parse_exact(c[1].as_bytes()));
        let got = match (a, b) {
            (Some(x), Some(y)) => {
                let mut out = Vec::new();
                (x + y).write_fixed(&mut out, 17);
                String::from_utf8(out).expect("ascii")
            }
            _ => "-".to_string(),
        };
        let read = (u8::from(a.is_some()).to_string(), u8::from(b.is_some()).to_string());
        if read != (c[2].to_string(), c[3].to_string()) || got != c[4] {
            let short = |s: &str| s.chars().take(60).collect::<String>();
            wrong.push(format!(
                "{:?} + {:?}: glibc {}{} {}, kevy {}{} {}",
                c[0],
                c[1],
                c[2],
                c[3],
                short(c[4]),
                read.0,
                read.1,
                short(&got)
            ));
        }
        checked += 1;
    }
    assert!(checked > 3000, "the table did not load");
    assert!(
        wrong.is_empty(),
        "{} of {checked} differ:\n{}",
        wrong.len(),
        wrong[..wrong.len().min(20)].join("\n")
    );
}
