//! `XINFO` over the wire, both protocols, against the replies a valkey
//! 9.1.2 server gave for the same commands; and the group state only
//! `XINFO` shows — the read counter and when each consumer was last handed
//! an entry — coming back from a restart as it was.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_testnet::free_port;

type Configure = Box<
    dyn FnOnce(kevy_rt::Runtime<kevy::KevyCommands>) -> kevy_rt::Runtime<kevy::KevyCommands> + Send,
>;

fn with_runtime(dir: &std::path::Path, nshards: usize, aof: bool, body: impl FnOnce(u16)) {
    let port = free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let dir = dir.to_path_buf();
    let configure: Configure = Box::new(move |rt| rt.with_aof(aof));
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
            .bind([127, 0, 0, 1], port)
            .shards(nshards)
            .with_data_dir(dir);
        configure(rt).run(stop_t).unwrap();
    });
    let up = (0..400).any(|_| {
        std::thread::sleep(Duration::from_millis(5));
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(up, "runtime did not start");
    body(port);
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

struct Conn(BufReader<std::net::TcpStream>);

impl Conn {
    fn open(port: u16) -> Conn {
        Conn(BufReader::new(std::net::TcpStream::connect(("127.0.0.1", port)).unwrap()))
    }

    /// The reply to `cmd`, split on spaces, as the bytes on the wire.
    fn call(&mut self, cmd: &str) -> String {
        let parts: Vec<&str> = cmd.split(' ').collect();
        let mut req = format!("*{}\r\n", parts.len());
        for p in parts {
            req.push_str(&format!("${}\r\n{p}\r\n", p.len()));
        }
        self.0.get_mut().write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        self.read_one(&mut out);
        String::from_utf8(out).unwrap()
    }

    /// One whole RESP2 or RESP3 reply.
    fn read_one(&mut self, out: &mut Vec<u8>) {
        let mut line = Vec::new();
        self.0.read_until(b'\n', &mut line).unwrap();
        out.extend_from_slice(&line);
        let n: i64 = std::str::from_utf8(&line[1..line.len() - 2]).unwrap().parse().unwrap_or(0);
        match line[0] {
            b'$' | b'=' | b'!' if n >= 0 => {
                let mut body = vec![0u8; n as usize + 2];
                self.0.read_exact(&mut body).unwrap();
                out.extend_from_slice(&body);
            }
            b'*' | b'~' | b'>' => (0..n.max(0)).for_each(|_| self.read_one(out)),
            b'%' | b'|' => (0..n.max(0) * 2).for_each(|_| self.read_one(out)),
            _ => {}
        }
    }
}

/// Errors, the stream head, its FULL form, and the groups' `entries-read`
/// and `lag` through a deletion and a trim, over RESP2.
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

/// The same over RESP3: named fields as a map, a missing value as a null.
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

/// Two shards, so the keys land on both and the RESP3 shapes are the
/// shard's, not only the first one's.
#[test]
fn xinfo_answers_as_valkey_does_over_both_protocols() {
    let dir = kevy_tmpdir::TmpDir::new("xinfo-wire");
    with_runtime(dir.path(), 2, false, |p| {
        let mut c = Conn::open(p);
        for (cmd, want) in RESP2 {
            assert_eq!(c.call(cmd), *want, "{cmd}");
        }
        let mut c = Conn::open(p);
        assert_eq!(c.call("FLUSHALL"), "+OK\r\n");
        assert!(c.call("HELLO 3").starts_with('%'));
        for (cmd, want) in RESP3 {
            assert_eq!(c.call(cmd), *want, "{cmd} over RESP3");
        }
    });
}

/// A stream whose groups hold every kind of read counter and consumer:
/// one read into and past, one set by `ENTRIESREAD`, one moved by
/// `SETID … ENTRIESREAD`, one left unknown by a deletion ahead of it; a
/// consumer that read, one that read with `NOACK`, one only made, one that
/// claimed, one whose claim took nothing. Waits between the steps keep
/// the times apart.
fn build_groups(c: &mut Conn) {
    for id in ["1-1", "2-1", "3-1", "4-1", "5-1"] {
        assert!(c.call(&format!("XADD s {id} f v")).starts_with('$'));
    }
    for cmd in ["XGROUP CREATE s g 0", "XGROUP CREATE s h $ ENTRIESREAD 2", "XGROUP CREATE s k 0"] {
        assert_eq!(c.call(cmd), "+OK\r\n", "{cmd}");
    }
    assert!(c.call("XREADGROUP GROUP g reader COUNT 2 STREAMS s >").starts_with("*1"));
    std::thread::sleep(Duration::from_millis(30));
    assert!(c.call("XREADGROUP GROUP g quiet COUNT 1 NOACK STREAMS s >").starts_with("*1"));
    assert_eq!(c.call("XGROUP CREATECONSUMER s g made"), ":1\r\n");
    std::thread::sleep(Duration::from_millis(30));
    assert!(c.call("XCLAIM s g claimer 0 1-1 JUSTID").starts_with("*1"));
    assert!(c.call("XAUTOCLAIM s g empty 99999999 0").starts_with("*3"));
    assert!(c.call("XREADGROUP GROUP k z COUNT 1 STREAMS s >").starts_with("*1"));
    assert_eq!(c.call("XGROUP SETID s k 1-1 ENTRIESREAD 7"), "+OK\r\n");
    assert_eq!(c.call("XDEL s 5-1"), ":1\r\n");
}

/// Everything FULL shows is state, not the clock: absolute times.
fn full(c: &mut Conn) -> String {
    c.call("XINFO STREAM s FULL")
}

/// What the state holds, so a comparison of two FULL replies compares
/// something: every consumer, the counters, a never-active consumer.
fn assert_built(full: &str) {
    for name in ["reader", "quiet", "made", "claimer", "empty", "z"] {
        assert!(full.contains(&format!("\r\n{name}\r\n$9\r\nseen-time")), "{name}: {full}");
    }
    let read = |g: &str, v: &str| format!("\r\n{g}\r\n$17\r\nlast-delivered-id\r\n{v}");
    assert!(
        full.contains(&read("g", "$3\r\n3-1\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n$-1"))
    );
    assert!(full.contains(&read("h", "$3\r\n5-1\r\n$12\r\nentries-read\r\n:2\r\n$3\r\nlag\r\n:0")));
    assert!(
        full.contains(&read("k", "$3\r\n1-1\r\n$12\r\nentries-read\r\n:7\r\n$3\r\nlag\r\n$-1"))
    );
    assert!(full.contains("\r\nmade\r\n$9\r\nseen-time\r\n:"), "{full}");
    assert!(
        full.matches("$11\r\nactive-time\r\n:-1\r\n").count() == 3,
        "made, quiet, empty: {full}"
    );
}

/// From the log as written, from the log `BGREWRITEAOF` compacts it to,
/// and from a snapshot, the group state comes back byte for byte.
#[test]
fn read_counters_and_active_times_survive_a_restart() {
    let dir = kevy_tmpdir::TmpDir::new("xinfo-restart");
    let mut want = String::new();
    with_runtime(dir.path(), 1, true, |p| {
        let mut c = Conn::open(p);
        build_groups(&mut c);
        want = full(&mut c);
    });
    assert_built(&want);
    with_runtime(dir.path(), 1, true, |p| {
        let mut c = Conn::open(p);
        assert_eq!(full(&mut c), want, "from the log");
        assert_eq!(c.call("BGREWRITEAOF"), "+OK\r\n");
        let aof = dir.path().join("aof-0.aof");
        let compacted = (0..1000).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            std::fs::read(&aof).is_ok_and(|b| b.windows(8).any(|w| w == b"MKSTREAM"))
        });
        assert!(compacted, "the rewritten AOF never swapped in");
    });
    with_runtime(dir.path(), 1, true, |p| {
        assert_eq!(full(&mut Conn::open(p)), want, "from the rewritten log");
    });

    let dir = kevy_tmpdir::TmpDir::new("xinfo-snapshot");
    with_runtime(dir.path(), 1, false, |p| {
        let mut c = Conn::open(p);
        build_groups(&mut c);
        want = full(&mut c);
        assert_eq!(c.call("SAVE"), "+OK\r\n");
    });
    assert!(dir.path().join("dump-0.rdb").exists(), "no snapshot was written");
    with_runtime(dir.path(), 1, false, |p| {
        assert_eq!(full(&mut Conn::open(p)), want, "from the snapshot");
    });
}
