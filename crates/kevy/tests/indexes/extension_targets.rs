//! An extension read goes to the shards `Commands::extension_targets`
//! names — in its first phase and in every follow-up phase — and to no
//! other. Each shard holds one marker key, so the per-shard half can say
//! which shard ran it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kevy_rt::{ArgvView, Commands, ExtensionReduced, Route, Store, TxnKind};

use super::common::Wire;

const SHARDS: usize = 4;

static RAN: [AtomicUsize; SHARDS] = [const { AtomicUsize::new(0) }; SHARDS];

fn marker(s: usize) -> Vec<u8> {
    (0..)
        .map(|i| format!("m{s}-{i}").into_bytes())
        .find(|k| kevy_rt::shard_of_key(k, SHARDS, false) == s)
        .unwrap()
}

#[derive(Clone)]
struct Counted;

impl Commands for Counted {
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route {
        if args[0].eq_ignore_ascii_case(b"EXT") {
            return Route::Extension;
        }
        Route::Single(1)
    }
    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        store.set(&args[1], b"1".to_vec(), None, false, false);
        b"+OK\r\n".to_vec()
    }
    fn is_quit<A: ArgvView + ?Sized>(&self, _: &A) -> bool {
        false
    }
    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        !args[0].eq_ignore_ascii_case(b"EXT")
    }
    fn txn_kind<A: ArgvView + ?Sized>(&self, _: &A) -> TxnKind {
        TxnKind::Other
    }
    fn extension_op(&self, store: &mut Store, _: &[Vec<u8>]) -> Vec<u8> {
        let s = (0..SHARDS).find(|&s| matches!(store.get(&marker(s)), Ok(Some(_)))).unwrap();
        RAN[s].fetch_add(1, Ordering::SeqCst);
        s.to_string().into_bytes()
    }
    /// `EXT <targets> <next targets>…`: each phase names its shards in
    /// its first argument and passes the rest on.
    fn extension_targets(&self, argv: &[Vec<u8>]) -> Option<Vec<usize>> {
        let list = std::str::from_utf8(argv.get(1)?).ok()?;
        Some(list.split(',').map(|s| s.parse().unwrap()).collect())
    }
    fn extension_reduce(
        &self,
        argv: &[Vec<u8>],
        mut chunks: Vec<Vec<u8>>,
        _: kevy_resp::RespVersion,
    ) -> ExtensionReduced {
        chunks.sort();
        let ran = chunks.join(&b","[..]);
        if argv.len() > 2 {
            let mut next = vec![argv[0].clone()];
            next.extend_from_slice(&argv[2..]);
            return ExtensionReduced::Continue(next);
        }
        ExtensionReduced::Reply(
            [b"$", ran.len().to_string().as_bytes(), b"\r\n", &ran, b"\r\n"].concat(),
        )
    }
}

#[test]
fn every_phase_runs_on_the_shards_it_names_and_no_other() {
    let port = kevy_testnet::free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_rt = stop.clone();
    let dir = kevy_tmpdir::TmpDir::new("ext-targets");
    let data = dir.path().to_path_buf();
    let rt = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(Counted)
            .bind([127, 0, 0, 1], port)
            .shards(SHARDS)
            .with_data_dir(data)
            .run(stop_rt)
            .unwrap();
    });
    kevy_testnet::assert_listening(port, "the runtime under test");
    let sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut w = Wire::new(sock);
    for s in 0..SHARDS {
        assert_eq!(w.call(&[b"SET".as_slice(), &marker(s)]), b"+OK\r\n");
    }
    // two shards, then one, then two others: the last phase replies with
    // the shards it ran on
    assert_eq!(w.call(&[b"EXT".as_slice(), b"1,2", b"3", b"0,2"]), b"$3\r\n0,2\r\n");
    let ran: Vec<usize> = RAN.iter().map(|r| r.load(Ordering::SeqCst)).collect();
    assert_eq!(ran, [1, 1, 2, 1], "shard 0 once, 1 once, 2 twice, 3 once");
    stop.store(true, Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    rt.join().unwrap();
}
