//! Every double here was given to Redis 8.10 and read back from ZSCORE; the
//! second column is the text Redis replied with.

#[test]
fn doubles_print_as_redis_prints_them() {
    let table = include_str!("fixtures/redis-doubles.tsv");
    let mut checked = 0;
    let mut wrong = Vec::new();
    for line in table.lines() {
        let (given, want) = line.split_once('\t').expect("two columns");
        let mut out = Vec::new();
        kevy_resp::write_double(&mut out, given.parse().expect("a double"));
        if out != want.as_bytes() {
            wrong.push(format!("{given}: redis {want}, kevy {}", String::from_utf8_lossy(&out)));
        }
        checked += 1;
    }
    assert!(checked > 500, "the table did not load");
    assert!(
        wrong.is_empty(),
        "{} of {checked} differ:\n{}",
        wrong.len(),
        wrong[..wrong.len().min(20)].join("\n")
    );
}
