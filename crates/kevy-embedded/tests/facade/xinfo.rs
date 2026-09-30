//! The embedded engine answers `XINFO` as the server does and as a valkey
//! 9.1.2 server did for the same commands, byte for byte.

use kevy_embedded::{Config, Store};

fn call(s: &Store, cmd: &str) -> String {
    let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    String::from_utf8(out).unwrap()
}

/// Errors, the stream head, its FULL form, and the groups' `entries-read`
/// and `lag` through a deletion and a trim.
const CORPUS: &[(&str, &str)] = &[
    ("XINFO STREAM nokey", "-ERR no such key\r\n"),
    ("XINFO GROUPS nokey", "-ERR no such key\r\n"),
    ("XINFO CONSUMERS nokey g", "-ERR no such key\r\n"),
    ("SET str v", "+OK\r\n"),
    ("XINFO STREAM str", "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"),
    (
        "XINFO STREAM str FULL",
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    ),
    ("XINFO GROUPS str", "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"),
    (
        "XINFO CONSUMERS str g",
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    ),
    ("XINFO", "-ERR wrong number of arguments for 'xinfo' command\r\n"),
    ("XINFO bogus", "-ERR unknown subcommand 'bogus'. Try XINFO HELP.\r\n"),
    ("XINFO STREAM", "-ERR wrong number of arguments for 'xinfo|stream' command\r\n"),
    ("XINFO GROUPS a b", "-ERR wrong number of arguments for 'xinfo|groups' command\r\n"),
    ("XINFO CONSUMERS s", "-ERR wrong number of arguments for 'xinfo|consumers' command\r\n"),
    (
        "XINFO HELP",
        "*9\r\n+XINFO <subcommand> [<arg> [value] [opt] ...]. Subcommands are:\r\n+CONSUMERS <key> <groupname>\r\n+    Show consumers of <groupname>.\r\n+GROUPS <key>\r\n+    Show the stream consumer groups.\r\n+STREAM <key> [FULL [COUNT <count>]\r\n+    Show information about the stream.\r\n+HELP\r\n+    Print this help.\r\n",
    ),
    ("XINFO help extra", "-ERR wrong number of arguments for 'xinfo|help' command\r\n"),
    ("XADD s 1-1 a 1", "$3\r\n1-1\r\n"),
    ("XADD s 2-1 b 2 c 3", "$3\r\n2-1\r\n"),
    ("XADD s 3-1 d 4", "$3\r\n3-1\r\n"),
    ("XINFO CONSUMERS s nogroup", "-NOGROUP No such consumer group 'nogroup' for key name 's'\r\n"),
    (
        "XINFO STREAM s",
        "*20\r\n$6\r\nlength\r\n:3\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n3-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:3\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-1\r\n$6\r\ngroups\r\n:0\r\n$11\r\nfirst-entry\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n$10\r\nlast-entry\r\n*2\r\n$3\r\n3-1\r\n*2\r\n$1\r\nd\r\n$1\r\n4\r\n",
    ),
    (
        "XINFO STREAM s full count 1",
        "*18\r\n$6\r\nlength\r\n:3\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n3-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:3\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-1\r\n$7\r\nentries\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n$6\r\ngroups\r\n*0\r\n",
    ),
    (
        "XINFO STREAM s FULL COUNT 0",
        "*18\r\n$6\r\nlength\r\n:3\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n3-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:3\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-1\r\n$7\r\nentries\r\n*3\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n*2\r\n$3\r\n2-1\r\n*4\r\n$1\r\nb\r\n$1\r\n2\r\n$1\r\nc\r\n$1\r\n3\r\n*2\r\n$3\r\n3-1\r\n*2\r\n$1\r\nd\r\n$1\r\n4\r\n$6\r\ngroups\r\n*0\r\n",
    ),
    ("XINFO STREAM s FULL COUNT x", "-ERR value is not an integer or out of range\r\n"),
    (
        "XINFO STREAM s FULL COUNT",
        "-ERR unknown subcommand or wrong number of arguments for 'STREAM'. Try XINFO HELP.\r\n",
    ),
    (
        "XINFO stream s bogus",
        "-ERR unknown subcommand or wrong number of arguments for 'stream'. Try XINFO HELP.\r\n",
    ),
    ("XINFO GROUPS s", "*0\r\n"),
    ("XGROUP CREATE s g0 0", "+OK\r\n"),
    ("XGROUP CREATE s g1 $", "+OK\r\n"),
    ("XGROUP CREATE s g2 0 ENTRIESREAD 1", "+OK\r\n"),
    (
        "XINFO GROUPS s",
        "*3\r\n*12\r\n$4\r\nname\r\n$2\r\ng0\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:3\r\n*12\r\n$4\r\nname\r\n$2\r\ng1\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-1\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$2\r\ng2\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n:2\r\n",
    ),
    (
        "XREADGROUP GROUP g0 alice COUNT 1 STREAMS s >",
        "*1\r\n*2\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n",
    ),
    (
        "XINFO GROUPS s",
        "*3\r\n*12\r\n$4\r\nname\r\n$2\r\ng0\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:1\r\n$17\r\nlast-delivered-id\r\n$3\r\n1-1\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$2\r\ng1\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-1\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$2\r\ng2\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n:2\r\n",
    ),
    (
        "XREADGROUP GROUP g0 bob STREAMS s >",
        "*1\r\n*2\r\n$1\r\ns\r\n*2\r\n*2\r\n$3\r\n2-1\r\n*4\r\n$1\r\nb\r\n$1\r\n2\r\n$1\r\nc\r\n$1\r\n3\r\n*2\r\n$3\r\n3-1\r\n*2\r\n$1\r\nd\r\n$1\r\n4\r\n",
    ),
    (
        "XINFO GROUPS s",
        "*3\r\n*12\r\n$4\r\nname\r\n$2\r\ng0\r\n$9\r\nconsumers\r\n:2\r\n$7\r\npending\r\n:3\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-1\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$2\r\ng1\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-1\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$2\r\ng2\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n:2\r\n",
    ),
    ("XDEL s 2-1", ":1\r\n"),
    (
        "XINFO GROUPS s",
        "*3\r\n*12\r\n$4\r\nname\r\n$2\r\ng0\r\n$9\r\nconsumers\r\n:2\r\n$7\r\npending\r\n:3\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-1\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$2\r\ng1\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-1\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$2\r\ng2\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n$-1\r\n",
    ),
    (
        "XINFO STREAM s",
        "*20\r\n$6\r\nlength\r\n:2\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n3-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n2-1\r\n$13\r\nentries-added\r\n:3\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-1\r\n$6\r\ngroups\r\n:3\r\n$11\r\nfirst-entry\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n$10\r\nlast-entry\r\n*2\r\n$3\r\n3-1\r\n*2\r\n$1\r\nd\r\n$1\r\n4\r\n",
    ),
    ("XADD e 1-1 a 1", "$3\r\n1-1\r\n"),
    ("XDEL e 1-1", ":1\r\n"),
    ("XGROUP CREATE e g 0", "+OK\r\n"),
    (
        "XINFO STREAM e",
        "*20\r\n$6\r\nlength\r\n:0\r\n$15\r\nradix-tree-keys\r\n:0\r\n$16\r\nradix-tree-nodes\r\n:1\r\n$17\r\nlast-generated-id\r\n$3\r\n1-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n1-1\r\n$13\r\nentries-added\r\n:1\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n0-0\r\n$6\r\ngroups\r\n:1\r\n$11\r\nfirst-entry\r\n$-1\r\n$10\r\nlast-entry\r\n$-1\r\n",
    ),
    ("XADD t 1-0 f v", "$3\r\n1-0\r\n"),
    ("XADD t 2-0 f v", "$3\r\n2-0\r\n"),
    ("XADD t 3-0 f v", "$3\r\n3-0\r\n"),
    ("XADD t 4-0 f v", "$3\r\n4-0\r\n"),
    ("XGROUP CREATE t g 0", "+OK\r\n"),
    ("XGROUP CREATE t h 0", "+OK\r\n"),
    (
        "XREADGROUP GROUP h x COUNT 3 STREAMS t >",
        "*1\r\n*2\r\n$1\r\nt\r\n*3\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    ("XTRIM t MAXLEN 2", ":2\r\n"),
    (
        "XINFO STREAM t",
        "*20\r\n$6\r\nlength\r\n:2\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n4-0\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:4\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n3-0\r\n$6\r\ngroups\r\n:2\r\n$11\r\nfirst-entry\r\n*2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n$10\r\nlast-entry\r\n*2\r\n$3\r\n4-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    (
        "XINFO GROUPS t",
        "*2\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$1\r\nh\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:3\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-0\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n:1\r\n",
    ),
];

/// Two shards, so the keys land on both.
#[test]
fn xinfo_answers_as_valkey_does() {
    let s = Store::open(Config::default().with_shards(2)).expect("open");
    for (cmd, want) in CORPUS {
        assert_eq!(call(&s, cmd), *want, "{cmd}");
    }
}

/// A consumer made by `CREATECONSUMER` was never handed an entry, so its
/// `inactive` is -1; one that read counts from the read.
#[test]
fn a_consumer_never_handed_an_entry_is_inactive_minus_one() {
    let s = Store::open(Config::default()).expect("open");
    for cmd in ["XADD s 1-1 f v", "XGROUP CREATE s g 0", "XGROUP CREATECONSUMER s g made"] {
        assert!(!call(&s, cmd).starts_with('-'), "{cmd}");
    }
    assert!(call(&s, "XREADGROUP GROUP g reader STREAMS s >").starts_with("*1"));
    let got = call(&s, "XINFO CONSUMERS s g");
    let t: Vec<&str> = got.split("\r\n").collect();
    let inactive = |name: &str| {
        let at = t.iter().position(|p| *p == name).unwrap();
        t[at + 7..].iter().position(|p| *p == "inactive").map(|i| t[at + 8 + i]).unwrap()
    };
    assert_eq!(inactive("made"), ":-1", "{got}");
    let reader: i64 = inactive("reader")[1..].parse().unwrap();
    assert!((0..1_000).contains(&reader), "{got}");
}
