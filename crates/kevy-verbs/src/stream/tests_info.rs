//! `XINFO` against the replies a valkey 9.1.2 server gave for the same
//! commands, byte for byte. Every reply here is independent of the clock;
//! the consumer times are checked on a store driven with explicit times.

use kevy_resp::{Argv, RespVersion};
use kevy_store::{
    AckMode, GroupCreateMode, MissingStream, ReadGroupId, Store, StreamId, XAddIdSpec,
};

fn argv(cmd: &str) -> Argv {
    Argv::from(cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>())
}

/// `cmd`'s reply, `XINFO` in the shapes of `proto` and anything else as
/// the shared layer answers it.
fn reply(store: &mut Store, cmd: &str, proto: RespVersion) -> String {
    let a = argv(cmd);
    let mut out = Vec::new();
    if a[0].eq_ignore_ascii_case(b"XINFO") {
        crate::cmd::xinfo(store, &a, &mut out, proto);
    } else {
        let mut buf = [0u8; 32];
        let up = crate::args::upper_verb(&a[0], &mut buf).to_vec();
        crate::exec(store, &up, &a, &mut out);
    }
    String::from_utf8(out).unwrap()
}

fn run_corpus(corpus: &[(&str, &str)], proto: RespVersion) {
    let mut s = Store::new();
    for (cmd, want) in corpus {
        assert_eq!(reply(&mut s, cmd, proto), *want, "{cmd}");
    }
}

/// Errors, the help text, the stream head and its FULL form, and the
/// groups' `entries-read` and `lag` through deletions, trims, `XSETID`,
/// `XGROUP SETID` and `ENTRIESREAD`. A Redis 8.10 server answers the same
/// but for `radix-tree-nodes` (its tree is shaped differently), six more
/// `XINFO STREAM` fields for an `XADD` option kevy does not have, and an
/// `ENTRIESREAD` above `entries-added`, which it lowers to that.
const RESP2: &[(&str, &str)] = &[
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
    (
        "XINFO STREAM e FULL",
        "*18\r\n$6\r\nlength\r\n:0\r\n$15\r\nradix-tree-keys\r\n:0\r\n$16\r\nradix-tree-nodes\r\n:1\r\n$17\r\nlast-generated-id\r\n$3\r\n1-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n1-1\r\n$13\r\nentries-added\r\n:1\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n0-0\r\n$7\r\nentries\r\n*0\r\n$6\r\ngroups\r\n*1\r\n*14\r\n$4\r\nname\r\n$1\r\ng\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n$9\r\npel-count\r\n:0\r\n$7\r\npending\r\n*0\r\n$9\r\nconsumers\r\n*0\r\n",
    ),
    ("XADD a 1-0 f v", "$3\r\n1-0\r\n"),
    ("XADD a 2-0 f v", "$3\r\n2-0\r\n"),
    ("XADD a 3-0 f v", "$3\r\n3-0\r\n"),
    ("XADD a 4-0 f v", "$3\r\n4-0\r\n"),
    ("XADD a 5-0 f v", "$3\r\n5-0\r\n"),
    ("XDEL a 1-0 2-0 3-0", ":3\r\n"),
    ("XGROUP CREATE a g00 0", "+OK\r\n"),
    ("XGROUP CREATE a g10 1-0", "+OK\r\n"),
    ("XGROUP CREATE a g30 3-0", "+OK\r\n"),
    ("XGROUP CREATE a g40 4-0", "+OK\r\n"),
    ("XGROUP CREATE a g45 4-5", "+OK\r\n"),
    ("XGROUP CREATE a g50 5-0", "+OK\r\n"),
    ("XGROUP CREATE a g90 9-0", "+OK\r\n"),
    ("XGROUP CREATE a e5 0 ENTRIESREAD 5", "+OK\r\n"),
    (
        "XINFO GROUPS a",
        "*8\r\n*12\r\n$4\r\nname\r\n$2\r\ne5\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:5\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$3\r\ng00\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$3\r\ng10\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n1-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n$-1\r\n*12\r\n$4\r\nname\r\n$3\r\ng30\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$3\r\ng40\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:1\r\n*12\r\n$4\r\nname\r\n$3\r\ng45\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-5\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n$-1\r\n*12\r\n$4\r\nname\r\n$3\r\ng50\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n5-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$3\r\ng90\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n9-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n$-1\r\n",
    ),
    (
        "XREADGROUP GROUP g10 x COUNT 1 STREAMS a >",
        "*1\r\n*2\r\n$1\r\na\r\n*1\r\n*2\r\n$3\r\n4-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    (
        "XREADGROUP GROUP e5 x COUNT 1 STREAMS a >",
        "*1\r\n*2\r\n$1\r\na\r\n*1\r\n*2\r\n$3\r\n4-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    (
        "XINFO GROUPS a",
        "*8\r\n*12\r\n$4\r\nname\r\n$2\r\ne5\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:1\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-0\r\n$12\r\nentries-read\r\n:4\r\n$3\r\nlag\r\n:1\r\n*12\r\n$4\r\nname\r\n$3\r\ng00\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$3\r\ng10\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:1\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-0\r\n$12\r\nentries-read\r\n:4\r\n$3\r\nlag\r\n:1\r\n*12\r\n$4\r\nname\r\n$3\r\ng30\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:2\r\n*12\r\n$4\r\nname\r\n$3\r\ng40\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:1\r\n*12\r\n$4\r\nname\r\n$3\r\ng45\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-5\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n$-1\r\n*12\r\n$4\r\nname\r\n$3\r\ng50\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n5-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n*12\r\n$4\r\nname\r\n$3\r\ng90\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n9-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n$-1\r\n",
    ),
    ("XADD d 1-0 f v", "$3\r\n1-0\r\n"),
    ("XADD d 2-0 f v", "$3\r\n2-0\r\n"),
    ("XADD d 3-0 f v", "$3\r\n3-0\r\n"),
    ("XADD d 4-0 f v", "$3\r\n4-0\r\n"),
    ("XGROUP CREATE d g 0", "+OK\r\n"),
    ("XGROUP SETID d g 2-0", "+OK\r\n"),
    (
        "XINFO GROUPS d",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n2-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n$-1\r\n",
    ),
    ("XGROUP SETID d g 2-0 ENTRIESREAD 10", "+OK\r\n"),
    (
        "XINFO GROUPS d",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n2-0\r\n$12\r\nentries-read\r\n:10\r\n$3\r\nlag\r\n:-6\r\n",
    ),
    ("XGROUP SETID d g $", "+OK\r\n"),
    (
        "XINFO GROUPS d",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n4-0\r\n$12\r\nentries-read\r\n$-1\r\n$3\r\nlag\r\n:0\r\n",
    ),
    ("XGROUP SETID d g 1-5 ENTRIESREAD 1", "+OK\r\n"),
    (
        "XREADGROUP GROUP g x COUNT 1 STREAMS d >",
        "*1\r\n*2\r\n$1\r\nd\r\n*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    (
        "XINFO GROUPS d",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:1\r\n$17\r\nlast-delivered-id\r\n$3\r\n2-0\r\n$12\r\nentries-read\r\n:2\r\n$3\r\nlag\r\n:2\r\n",
    ),
    (
        "XGROUP SETID d g 2-0 ENTRIESREAD -2",
        "-ERR value for ENTRIESREAD must be positive or -1\r\n",
    ),
    ("XGROUP SETID d g 2-0 ENTRIESREAD x", "-ERR value is not an integer or out of range\r\n"),
    (
        "XGROUP SETID d g 2-0 ENTRIESREAD 1 ENTRIESREAD 2",
        "-ERR unknown subcommand or wrong number of arguments for 'SETID'. Try XGROUP HELP.\r\n",
    ),
    ("XGROUP SETID d nog 0", "-NOGROUP No such consumer group 'nog' for key name 'd'\r\n"),
    (
        "XGROUP SETID nokey g 0",
        "-ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.\r\n",
    ),
    (
        "XGROUP CREATE d h 0 ENTRIESREAD 3 MKSTREAM extra",
        "-ERR unknown subcommand or wrong number of arguments for 'CREATE'. Try XGROUP HELP.\r\n",
    ),
    ("XGROUP CREATE m g 0 ENTRIESREAD 3 MKSTREAM", "+OK\r\n"),
    (
        "XINFO GROUPS m",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n:0\r\n",
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
    ("XADD f 1-0 f v", "$3\r\n1-0\r\n"),
    ("XADD f 2-0 f v", "$3\r\n2-0\r\n"),
    ("XADD f 3-0 f v", "$3\r\n3-0\r\n"),
    ("XGROUP CREATE f g 0", "+OK\r\n"),
    (
        "XREADGROUP GROUP g x COUNT 1 STREAMS f >",
        "*1\r\n*2\r\n$1\r\nf\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    ("XSETID f 3-0 ENTRIESADDED 10 MAXDELETEDID 2-5", "+OK\r\n"),
    (
        "XINFO GROUPS f",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:1\r\n$17\r\nlast-delivered-id\r\n$3\r\n1-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n$-1\r\n",
    ),
    ("XADD f 10-0 f v", "$4\r\n10-0\r\n"),
    (
        "XREADGROUP GROUP g x COUNT 10 STREAMS f >",
        "*1\r\n*2\r\n$1\r\nf\r\n*3\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*2\r\n$4\r\n10-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    ),
    (
        "XINFO GROUPS f",
        "*1\r\n*12\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:1\r\n$7\r\npending\r\n:4\r\n$17\r\nlast-delivered-id\r\n$4\r\n10-0\r\n$12\r\nentries-read\r\n:11\r\n$3\r\nlag\r\n:0\r\n",
    ),
    (
        "XINFO STREAM m FULL",
        "*18\r\n$6\r\nlength\r\n:0\r\n$15\r\nradix-tree-keys\r\n:0\r\n$16\r\nradix-tree-nodes\r\n:1\r\n$17\r\nlast-generated-id\r\n$3\r\n0-0\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:0\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n0-0\r\n$7\r\nentries\r\n*0\r\n$6\r\ngroups\r\n*1\r\n*14\r\n$4\r\nname\r\n$1\r\ng\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n:0\r\n$9\r\npel-count\r\n:0\r\n$7\r\npending\r\n*0\r\n$9\r\nconsumers\r\n*0\r\n",
    ),
];

/// The same replies under RESP3: named fields as a map, a missing value as
/// a null.
const RESP3: &[(&str, &str)] = &[
    ("XINFO STREAM nokey", "-ERR no such key\r\n"),
    ("XINFO CONSUMERS nokey g", "-ERR no such key\r\n"),
    ("XADD s 1-1 a 1", "$3\r\n1-1\r\n"),
    ("XADD s 2-1 b 2 c 3", "$3\r\n2-1\r\n"),
    ("XGROUP CREATE s g1 $", "+OK\r\n"),
    ("XGROUP CREATE s g0 0 ENTRIESREAD 1", "+OK\r\n"),
    (
        "XINFO STREAM s",
        "%10\r\n$6\r\nlength\r\n:2\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n2-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:2\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-1\r\n$6\r\ngroups\r\n:2\r\n$11\r\nfirst-entry\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n$10\r\nlast-entry\r\n*2\r\n$3\r\n2-1\r\n*4\r\n$1\r\nb\r\n$1\r\n2\r\n$1\r\nc\r\n$1\r\n3\r\n",
    ),
    (
        "XINFO STREAM s FULL",
        "%9\r\n$6\r\nlength\r\n:2\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n2-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:2\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-1\r\n$7\r\nentries\r\n*2\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n*2\r\n$3\r\n2-1\r\n*4\r\n$1\r\nb\r\n$1\r\n2\r\n$1\r\nc\r\n$1\r\n3\r\n$6\r\ngroups\r\n*2\r\n%7\r\n$4\r\nname\r\n$2\r\ng0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n:1\r\n$9\r\npel-count\r\n:0\r\n$7\r\npending\r\n*0\r\n$9\r\nconsumers\r\n*0\r\n%7\r\n$4\r\nname\r\n$2\r\ng1\r\n$17\r\nlast-delivered-id\r\n$3\r\n2-1\r\n$12\r\nentries-read\r\n_\r\n$3\r\nlag\r\n:0\r\n$9\r\npel-count\r\n:0\r\n$7\r\npending\r\n*0\r\n$9\r\nconsumers\r\n*0\r\n",
    ),
    (
        "XINFO GROUPS s",
        "*2\r\n%6\r\n$4\r\nname\r\n$2\r\ng0\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n:1\r\n%6\r\n$4\r\nname\r\n$2\r\ng1\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n2-1\r\n$12\r\nentries-read\r\n_\r\n$3\r\nlag\r\n:0\r\n",
    ),
    ("XINFO CONSUMERS s g0", "*0\r\n"),
    ("XINFO CONSUMERS s nogroup", "-NOGROUP No such consumer group 'nogroup' for key name 's'\r\n"),
    (
        "XINFO HELP",
        "*9\r\n+XINFO <subcommand> [<arg> [value] [opt] ...]. Subcommands are:\r\n+CONSUMERS <key> <groupname>\r\n+    Show consumers of <groupname>.\r\n+GROUPS <key>\r\n+    Show the stream consumer groups.\r\n+STREAM <key> [FULL [COUNT <count>]\r\n+    Show information about the stream.\r\n+HELP\r\n+    Print this help.\r\n",
    ),
    ("XADD e 1-1 a 1", "$3\r\n1-1\r\n"),
    ("XDEL e 1-1", ":1\r\n"),
    (
        "XINFO STREAM e",
        "%10\r\n$6\r\nlength\r\n:0\r\n$15\r\nradix-tree-keys\r\n:0\r\n$16\r\nradix-tree-nodes\r\n:1\r\n$17\r\nlast-generated-id\r\n$3\r\n1-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n1-1\r\n$13\r\nentries-added\r\n:1\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n0-0\r\n$6\r\ngroups\r\n:0\r\n$11\r\nfirst-entry\r\n_\r\n$10\r\nlast-entry\r\n_\r\n",
    ),
    (
        "XINFO STREAM e FULL COUNT 3",
        "%9\r\n$6\r\nlength\r\n:0\r\n$15\r\nradix-tree-keys\r\n:0\r\n$16\r\nradix-tree-nodes\r\n:1\r\n$17\r\nlast-generated-id\r\n$3\r\n1-1\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n1-1\r\n$13\r\nentries-added\r\n:1\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n0-0\r\n$7\r\nentries\r\n*0\r\n$6\r\ngroups\r\n*0\r\n",
    ),
    ("XDEL s 2-1", ":1\r\n"),
    (
        "XINFO GROUPS s",
        "*2\r\n%6\r\n$4\r\nname\r\n$2\r\ng0\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n0-0\r\n$12\r\nentries-read\r\n:1\r\n$3\r\nlag\r\n_\r\n%6\r\n$4\r\nname\r\n$2\r\ng1\r\n$9\r\nconsumers\r\n:0\r\n$7\r\npending\r\n:0\r\n$17\r\nlast-delivered-id\r\n$3\r\n2-1\r\n$12\r\nentries-read\r\n_\r\n$3\r\nlag\r\n:0\r\n",
    ),
];

#[test]
fn xinfo_answers_as_valkey_does() {
    run_corpus(RESP2, RespVersion::V2);
}

#[test]
fn xinfo_answers_as_valkey_does_over_resp3() {
    run_corpus(RESP3, RespVersion::V3);
}

/// Four entries on `s`, a group `g` from the start.
fn four_entries() -> Store {
    let mut s = Store::new();
    for ms in 1..=4 {
        let f = vec![(b"f".to_vec(), b"v".to_vec())];
        s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)
            .unwrap();
    }
    s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)
        .unwrap();
    s
}

/// `(seen-time, active-time)` of `consumer` in group `g`.
fn times(s: &Store, consumer: &[u8]) -> (u64, Option<u64>) {
    let c = s.stream_group_peek(b"s", b"g").unwrap().consumer(consumer).unwrap();
    (c.last_seen_ms(), c.last_active_ms())
}

/// Which commands move a consumer's `seen-time` and which its
/// `active-time`, as measured on valkey 9.1.2 and Redis 8.10: every group
/// read and every claim is contact, whatever it returns; only a read that
/// puts entries in the pending list, or a claim that takes one, is
/// activity. `XACK` and a repeated `CREATECONSUMER` are neither.
#[test]
fn seen_and_active_times_move_with_the_commands_that_move_them() {
    let mut s = four_entries();
    let new = ReadGroupId::New;
    s.xgroup_create_consumer(b"s", b"g", b"made", 10).unwrap();
    s.xreadgroup(b"s", b"g", b"reader", new, Some(1), AckMode::Pending, 20).unwrap();
    s.xreadgroup(b"s", b"g", b"noack", new, Some(1), AckMode::NoAck, 30).unwrap();
    s.xreadgroup(b"s", b"g", b"other", new, None, AckMode::Pending, 35).unwrap();
    // nothing new: contact, not activity
    s.xreadgroup(b"s", b"g", b"reader", new, None, AckMode::Pending, 50).unwrap();
    let history = ReadGroupId::ReplayAfter(StreamId::MIN);
    s.xreadgroup(b"s", b"g", b"other", history, None, AckMode::Pending, 55).unwrap();
    let opts = kevy_store::XClaimOpts::default();
    s.xclaim(b"s", b"g", b"claimer", &[StreamId::new(1, 0)], &opts, 60).unwrap();
    s.xclaim(b"s", b"g", b"made", &[StreamId::new(9, 0)], &opts, 70).unwrap();
    let mode = kevy_store::ClaimMode::Deliver;
    s.xautoclaim(b"s", b"g", b"auto", u64::MAX, StreamId::MIN, 10, mode, 80).unwrap();
    s.xgroup_create_consumer(b"s", b"g", b"made", 85).unwrap();
    s.xack(b"s", b"g", &[StreamId::new(3, 0)]).unwrap();
    assert_eq!(times(&s, b"made"), (70, None), "a claim that took nothing");
    assert_eq!(times(&s, b"reader"), (50, Some(20)), "an empty read");
    assert_eq!(times(&s, b"noack"), (30, None), "NOACK keeps nothing pending");
    assert_eq!(times(&s, b"other"), (55, Some(35)), "a history read, then an ack");
    assert_eq!(times(&s, b"claimer"), (60, Some(60)));
    assert_eq!(times(&s, b"auto"), (80, None));
    // FULL shows the times as they are; a consumer never active is -1
    let full = reply(&mut s, "XINFO STREAM s FULL", RespVersion::V2);
    let row = |name: &str, seen: &str, active: &str| {
        format!(
            "$4\r\nname\r\n${}\r\n{name}\r\n$9\r\nseen-time\r\n:{seen}\r\n\
             $11\r\nactive-time\r\n:{active}\r\n",
            name.len()
        )
    };
    for want in [row("made", "70", "-1"), row("reader", "50", "20"), row("claimer", "60", "60")] {
        assert!(full.contains(&want), "{want:?} in {full:?}");
    }
}

/// `idle` counts from the last contact and `inactive` from the last
/// activity; a consumer never active is `-1`.
#[test]
fn idle_and_inactive_count_from_seen_and_active() {
    let mut s = four_entries();
    let now = kevy_store::now_unix_ms();
    s.xgroup_create_consumer(b"s", b"g", b"made", now - 5_000).unwrap();
    let new = ReadGroupId::New;
    s.xreadgroup(b"s", b"g", b"busy", new, Some(1), AckMode::Pending, now - 9_000).unwrap();
    let history = ReadGroupId::ReplayAfter(StreamId::MIN);
    s.xreadgroup(b"s", b"g", b"busy", history, None, AckMode::Pending, now - 3_000).unwrap();
    let got = reply(&mut s, "XINFO CONSUMERS s g", RespVersion::V2);
    let t: Vec<&str> = got.split("\r\n").collect();
    let field = |name: &str, key: &str| -> i64 {
        let at = t.iter().position(|p| *p == name).unwrap();
        let k = (at..t.len()).find(|&i| t[i] == key).unwrap();
        t[k + 1][1..].parse().unwrap()
    };
    assert_eq!(field("made", "inactive"), -1);
    let (idle, inactive) = (field("busy", "idle"), field("busy", "inactive"));
    assert!((3_000..4_000).contains(&idle), "{got}");
    assert!((9_000..10_000).contains(&inactive), "{got}");
    assert!(t.starts_with(&["*2", "*8", "$4", "name"]), "{got}");
}

/// `COUNT` bounds the entries, each group's pending list and each
/// consumer's; 0 or less lifts the bound, and FULL alone is `COUNT 10`.
#[test]
fn full_count_bounds_every_list() {
    let mut s = Store::new();
    for ms in 1..=12 {
        let f = vec![(b"f".to_vec(), b"v".to_vec())];
        s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)
            .unwrap();
    }
    s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)
        .unwrap();
    s.xreadgroup(b"s", b"g", b"a", ReadGroupId::New, None, AckMode::Pending, 7).unwrap();
    // (entries, the group's pending rows, the consumer's pending rows)
    let rows = |cmd: &str, s: &mut Store| {
        let got = reply(s, cmd, RespVersion::V2);
        let entries = got.matches("*2\r\n$1\r\nf\r\n").count();
        // a group row names its owner, a consumer row does not
        let pending = got.matches("\r\n$1\r\na\r\n:7\r\n:1\r\n").count();
        let held = got.matches("\r\n:7\r\n:1\r\n").count() - pending;
        (entries, pending, held)
    };
    assert_eq!(rows("XINFO STREAM s FULL", &mut s), (10, 10, 10));
    assert_eq!(rows("XINFO STREAM s FULL COUNT 3", &mut s), (3, 3, 3));
    assert_eq!(rows("XINFO STREAM s FULL COUNT 0", &mut s), (12, 12, 12));
    assert_eq!(rows("XINFO STREAM s FULL COUNT -4", &mut s), (12, 12, 12));
}
