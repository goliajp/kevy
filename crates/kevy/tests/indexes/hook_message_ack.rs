//! A write whose hook sends a message to another shard does not reply
//! until that shard has applied it. The receiving shard here takes 200 ms
//! to apply, so a read sent the moment the write's reply arrives tells a
//! held reply (the read sees the message's effect) from one that went out
//! at once (it does not). A real global index applies in microseconds, and
//! its own end-to-end test cannot tell the two apart.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kevy_rt::{ArgvView, Commands, Route, Store, TxnKind};

use super::common::Wire;

const SHARDS: usize = 3;

thread_local! {
    static OUTBOX: RefCell<Vec<(usize, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
}

/// This round's keys, read on every shard's thread: the write's, and the
/// flag's, on different shards.
static KEYS: Mutex<(Vec<u8>, Vec<u8>)> = Mutex::new((Vec::new(), Vec::new()));

/// Keys with the write on shard `w` and the flag on shard `f`.
fn keys_for(w: usize, f: usize) -> (Vec<u8>, Vec<u8>) {
    let at = |k: &[u8]| kevy_rt::shard_of_key(k, SHARDS, false);
    let pick = |p: &str, s: usize| {
        (0..).map(|i| format!("{p}{i}").into_bytes()).find(|k| at(k) == s).unwrap()
    };
    (pick("w", w), pick("f", f))
}

fn keys() -> (Vec<u8>, Vec<u8>) {
    KEYS.lock().unwrap().clone()
}

#[derive(Clone)]
struct Slow {
    applied: Arc<AtomicUsize>,
}

impl Commands for Slow {
    fn route<A: ArgvView + ?Sized>(&self, _: &A) -> Route {
        Route::Single(1)
    }
    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        if args[0].eq_ignore_ascii_case(b"SET") {
            store.set(&args[1], args[2].to_vec(), None, false, false);
            return b"+OK\r\n".to_vec();
        }
        match store.get(&args[1]) {
            Ok(Some(v)) => [format!("${}\r\n", v.len()).as_bytes(), &v, b"\r\n"].concat(),
            _ => b"$-1\r\n".to_vec(),
        }
    }
    fn is_quit<A: ArgvView + ?Sized>(&self, _: &A) -> bool {
        false
    }
    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        args[0].eq_ignore_ascii_case(b"SET")
    }
    fn txn_kind<A: ArgvView + ?Sized>(&self, _: &A) -> TxnKind {
        TxnKind::Other
    }
    fn on_write(&self, _: &mut Store, key: &[u8]) {
        let (write, flag) = keys();
        if key == write {
            let to = kevy_rt::shard_of_key(&flag, SHARDS, false);
            OUTBOX.with(|o| o.borrow_mut().push((to, b"set".to_vec())));
        }
    }
    fn take_ext_out(&self) -> Vec<(usize, Vec<u8>)> {
        OUTBOX.with(|o| std::mem::take(&mut *o.borrow_mut()))
    }
    fn apply_ext(&self, store: &mut Store, _: &[u8]) {
        std::thread::sleep(std::time::Duration::from_millis(200));
        store.set(&keys().1, b"up".to_vec(), None, false, false);
        self.applied.fetch_add(1, Ordering::SeqCst);
    }
}

/// Send `cmds` in one write — a pipeline — and read one reply each.
fn pipeline(sock: &mut std::net::TcpStream, cmds: &[&[&[u8]]]) -> Vec<Vec<u8>> {
    use std::io::{Read, Write};
    let mut out = Vec::new();
    for parts in cmds {
        out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
        for p in *parts {
            out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            out.extend_from_slice(p);
            out.extend_from_slice(b"\r\n");
        }
    }
    sock.write_all(&out).unwrap();
    let (mut buf, mut replies) = (Vec::new(), Vec::new());
    while replies.len() < cmds.len() {
        if let Some(n) = super::common::reply_len(&buf) {
            replies.push(buf.drain(..n).collect());
            continue;
        }
        let mut chunk = [0u8; 4096];
        let got = sock.read(&mut chunk).unwrap();
        assert!(got > 0, "closed mid-pipeline");
        buf.extend_from_slice(&chunk[..got]);
    }
    replies
}

#[test]
fn a_write_replies_only_once_its_hook_message_is_applied() {
    let port = kevy_testnet::free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let applied = Arc::new(AtomicUsize::new(0));
    let cmds = Slow { applied: applied.clone() };
    let stop_rt = stop.clone();
    let dir = kevy_tmpdir::TmpDir::new("hook-ack");
    let data = dir.path().to_path_buf();
    let rt = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(cmds)
            .bind([127, 0, 0, 1], port)
            .shards(SHARDS)
            .with_data_dir(data)
            .run(stop_rt)
            .unwrap();
    });
    kevy_testnet::assert_listening(port, "the runtime under test");
    // every placement of the write and the flag: wherever the connection
    // lands, some placement serves the write there (the inline reply), some
    // forward it to the flag's shard, and some forward it to a third shard
    // — the one where a reply sent at once would overtake the message
    let placements =
        (0..SHARDS).flat_map(|w| (0..SHARDS).filter(move |&f| f != w).map(move |f| (w, f)));
    for (round, (ws, fs)) in placements.enumerate() {
        let (write, flag) = keys_for(ws, fs);
        *KEYS.lock().unwrap() = (write.clone(), flag.clone());
        let sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut w = Wire::new(sock);
        assert_eq!(w.call(&[b"SET".as_slice(), &flag, b"down"]), b"+OK\r\n");
        let before = applied.load(Ordering::SeqCst);
        assert_eq!(w.call(&[b"SET".as_slice(), &write, b"v"]), b"+OK\r\n");
        assert_eq!(
            applied.load(Ordering::SeqCst),
            before + 1,
            "round {round}: replied before the apply"
        );
        assert_eq!(w.call(&[b"GET".as_slice(), &flag]), b"$2\r\nup\r\n", "round {round}");
        // the same behind a command still pending on the connection (the
        // non-inline local path), in one pipeline
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let before = applied.load(Ordering::SeqCst);
        let set_flag: &[&[u8]] = &[b"SET", &flag, b"down"];
        let set_write: &[&[u8]] = &[b"SET", &write, b"v"];
        let replies = pipeline(&mut sock, &[set_flag, set_write]);
        assert_eq!(replies, [b"+OK\r\n".to_vec(), b"+OK\r\n".to_vec()]);
        assert_eq!(applied.load(Ordering::SeqCst), before + 1, "round {round}, pipelined");
    }
    stop.store(true, Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    rt.join().unwrap();
}
