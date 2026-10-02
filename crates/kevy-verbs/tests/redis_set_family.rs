//! `SET`'s options, `GETEX`, `DIGEST`, the `EXPIRE` family's conditions
//! and `LPOP`/`RPOP` with a count of 0, against the replies Redis 8.10.2
//! gave to the same commands. Each blank-line-separated group starts on an
//! empty store.

fn run_group(group: &str) -> usize {
    let mut store = kevy_store::Store::new();
    let mut checked = 0;
    for line in group.lines() {
        let (cmd, want) = line.split_once("\t=>\t").expect("a command, then its reply");
        let argv: Vec<Vec<u8>> = cmd.split('\t').map(|a| a.as_bytes().to_vec()).collect();
        let verb = argv[0].to_ascii_uppercase();
        let mut out = Vec::new();
        kevy_verbs::exec(&mut store, &verb, &kevy_resp::Argv::from(argv), &mut out);
        let want = want.replace("\\r\\n", "\r\n");
        assert_eq!(String::from_utf8_lossy(&out), want, "{cmd}");
        checked += 1;
    }
    checked
}

#[test]
fn replies_match_redis() {
    let table = include_str!("data/redis_set_family.txt");
    let checked: usize = table.split("\n\n").map(run_group).sum();
    assert_eq!(checked, 269);
}
